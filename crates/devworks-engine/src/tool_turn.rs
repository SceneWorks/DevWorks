//! Host-owned framing of provider tool arguments. A provider chooses arguments only; the
//! selected operation, mode, request identity and contract envelope come from the host.
use crate::{providers::schemas::RegisteredSchemas, runtime::Runtime};
use devworks_core::{
    admission::{self, Selection},
    context::{InvocationContext, PrincipalKind},
    fingerprint::sha256_hex,
    ports::{IdKind, storage::*},
    protocol::{
        AttemptId, CONTRACT, CommandReply, Id, OperationId, RetryAdvice, Revision,
        provider::{
            ArtifactRef, CompleteToolCall, NativeBinding, ToolCallKey, ToolOutcome, TranscriptBody,
        },
        results::{DomainCommandReply, DomainDecision, DomainQueryReply},
        support::DomainErrorCode,
    },
    providers::context::{ContentBlock, Message, MessageRole, ProviderContext, ResultBlock},
    registry::{self, Envelope, Mode, OperationDescriptor, SchemaRevision},
    services::{
        artifacts::{ArtifactService, host::HostArtifactPublication, version_ref},
        sessions::{
            transcript::{BatchOutcome, SessionBatch, SessionWriter},
            turn,
        },
    },
};
use serde_json::{Value, json};
use std::sync::Arc;

/// One closed call recovered from the transcript. Its result may have completed before a
/// lower-ordinal sibling, but provider continuation must emit it in the recorded call order.
pub struct ResolvedToolCall {
    pub call: CompleteToolCall,
    pub outcome: ToolOutcome,
    pub response_sequence: i64,
}

pub(crate) struct TurnSource<'a> {
    pub session_id: &'a Id,
    pub attempt_id: &'a AttemptId,
    pub after_sequence: i64,
    pub has_tools: bool,
}

pub(crate) struct ContinuationMaterial {
    pub context: ProviderContext,
    pub observed_sequence: i64,
}

fn authorized_session(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    session_id: &Id,
) -> Result<StoredSession, StorageError> {
    let session = read
        .sessions()
        .session(session_id)?
        .ok_or(StorageError::ScopeDenied)?;
    if ctx.session_id() != Some(session_id)
        || session.closed()
        || read.stamp().store_id != *ctx.store_id()
        || read.stamp().restore_epoch != ctx.restore_epoch().get()
        || !matches!(
            ctx.principal_kind(),
            PrincipalKind::Agent | PrincipalKind::Host
        )
        || read.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
    {
        return Err(StorageError::ScopeDenied);
    }
    if ctx.principal_kind() == PrincipalKind::Agent {
        let binding = read
            .authority()
            .principal(ctx.principal())?
            .ok_or(StorageError::ScopeDenied)?;
        if !binding.active
            || session.agent_id != *ctx.principal()
            || binding
                .project_id
                .as_ref()
                .is_some_and(|project| session.project_id.as_ref() != Some(project))
        {
            return Err(StorageError::ScopeDenied);
        }
    }
    Ok(session)
}

/// Enumerate unanswered calls only after checking the current session and principal binding.
/// The dispatcher must not expose even call identities from a foreign session.
pub(crate) fn pending_calls(
    store: &dyn ApplicationStore,
    ctx: &InvocationContext,
    session_id: &Id,
) -> Result<Vec<ToolCallKey>, StorageError> {
    read_snapshot(store, &ReadOptions::default(), |read| {
        authorized_session(read, ctx, session_id)?;
        let pending = read.sessions().pending_calls(session_id, 500)?;
        if pending.len() == 500 {
            return Err(invalid("pending tool calls exceed dispatch scan bound"));
        }
        pending
            .into_iter()
            .map(|row| {
                Ok(ToolCallKey {
                    attempt_id: AttemptId(row.attempt_id),
                    native_call_id: row.native_call_id,
                    ordinal: devworks_core::protocol::Counter::new(row.ordinal).map_err(invalid)?,
                })
            })
            .collect()
    })
}

/// Linked operations reconcile independently of unanswered sibling calls. Only the authorized
/// session's operation identities can be returned.
pub(crate) fn linked_operations(
    store: &dyn ApplicationStore,
    ctx: &InvocationContext,
    session_id: &Id,
) -> Result<Vec<StoredToolCall>, StorageError> {
    read_snapshot(store, &ReadOptions::default(), |read| {
        authorized_session(read, ctx, session_id)?;
        let linked = read.sessions().linked_operations(session_id, 500)?;
        if linked.len() == 500 {
            return Err(invalid("linked tool operations exceed scan bound"));
        }
        Ok(linked)
    })
}

/// Reconstruct a provider turn from durable entries, never from worker completion order. A
/// pending response leaves the entire turn unassembled while already completed siblings remain
/// durable and eligible for recovery. `after_sequence` is the host's position before dispatch.
pub(crate) fn resolved_turn(
    store: &dyn ApplicationStore,
    ctx: &InvocationContext,
    session_id: &Id,
    attempt_id: &AttemptId,
    after_sequence: i64,
) -> Result<Vec<ResolvedToolCall>, StorageError> {
    if after_sequence < 0 || ctx.session_id() != Some(session_id) {
        return Err(StorageError::ScopeDenied);
    }
    read_snapshot(store, &ReadOptions::default(), |read| {
        let session = read
            .sessions()
            .session(session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if session.closed()
            || read.stamp().store_id != *ctx.store_id()
            || read.stamp().restore_epoch != ctx.restore_epoch().get()
            || !matches!(
                ctx.principal_kind(),
                PrincipalKind::Agent | PrincipalKind::Host
            )
            || (ctx.principal_kind() == PrincipalKind::Agent
                && session.agent_id != *ctx.principal())
            || read.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
            || after_sequence >= session.next_sequence
        {
            return Err(StorageError::ScopeDenied);
        }
        if ctx.principal_kind() == PrincipalKind::Agent {
            let binding = read
                .authority()
                .principal(ctx.principal())?
                .ok_or(StorageError::ScopeDenied)?;
            if !binding.active
                || binding
                    .project_id
                    .as_ref()
                    .is_some_and(|project| session.project_id.as_ref() != Some(project))
            {
                return Err(StorageError::ScopeDenied);
            }
        }
        let mut position = after_sequence;
        let mut calls = Vec::new();
        // A turn larger than this is a visible recovery hold, never a silently truncated result.
        while position < session.last_durable_sequence() {
            if position - after_sequence >= 10_000 {
                return Err(invalid("tool turn exceeds transcript scan bound"));
            }
            let page = read.sessions().entries(session_id, position, 500)?;
            if page.is_empty() {
                return Err(invalid("tool turn transcript gap"));
            }
            for entry in &page {
                if entry.sequence != position + 1 {
                    return Err(invalid("tool turn transcript gap"));
                }
                position = entry.sequence;
                if entry.kind != "ToolCallRecorded" {
                    continue;
                }
                let TranscriptBody::ToolCallRecorded { tool_call } =
                    serde_json::from_value(entry.body.clone()).map_err(invalid)?
                else {
                    return Err(invalid("tool call entry kind mismatch"));
                };
                if &tool_call.call.attempt_id == attempt_id {
                    calls.push(tool_call);
                }
            }
        }
        if calls.is_empty() {
            return Err(StorageError::ScopeDenied);
        }
        calls.sort_by_key(|call| call.call.ordinal.get());
        if calls
            .windows(2)
            .any(|pair| pair[0].call.ordinal == pair[1].call.ordinal)
        {
            return Err(invalid("duplicate tool call ordinal"));
        }
        calls
            .into_iter()
            .map(|call| {
                let row = read
                    .sessions()
                    .tool_call(
                        session_id,
                        &call.call.attempt_id.0,
                        &call.call.native_call_id,
                    )?
                    .ok_or(StorageError::ScopeDenied)?;
                let sequence = row.response_sequence.ok_or_else(|| {
                    StorageError::unavailable(
                        Unavailable::UnsupportedRuntime,
                        "tool turn has unanswered calls",
                    )
                })?;
                if row.ordinal != call.call.ordinal.get() || sequence <= row.call_sequence {
                    return Err(invalid("tool response position mismatch"));
                }
                let entry = read
                    .sessions()
                    .entries(session_id, sequence - 1, 1)?
                    .into_iter()
                    .next()
                    .filter(|entry| entry.sequence == sequence)
                    .ok_or_else(|| invalid("tool response entry missing"))?;
                let TranscriptBody::ToolResponseRecorded {
                    call: response_key,
                    outcome,
                } = serde_json::from_value(entry.body).map_err(invalid)?
                else {
                    return Err(invalid("tool response entry kind mismatch"));
                };
                if response_key.attempt_id != call.call.attempt_id
                    || response_key.native_call_id != call.call.native_call_id
                    || response_key.ordinal != call.call.ordinal
                {
                    return Err(invalid("tool response identity mismatch"));
                }
                Ok(ResolvedToolCall {
                    call,
                    outcome,
                    response_sequence: sequence,
                })
            })
            .collect()
    })
}

/// Append one settled provider exchange to the exact immutable request context that produced it.
/// The assistant's text, opaque native items and calls retain their transcript order, while
/// results use call ordinal. Encoders then preserve each endpoint's required native structure.
pub(crate) fn continuation_context(
    store: &dyn ApplicationStore,
    artifacts: &Arc<ArtifactService>,
    ctx: &InvocationContext,
    source: TurnSource<'_>,
    binding: &NativeBinding,
    previous: &devworks_core::protocol::provider::ArtifactRef,
) -> Result<ContinuationMaterial, StorageError> {
    let TurnSource {
        session_id,
        attempt_id,
        after_sequence,
        has_tools,
    } = source;
    let resolved = if has_tools {
        resolved_turn(store, ctx, session_id, attempt_id, after_sequence)?
    } else {
        Vec::new()
    };
    let base = artifacts
        .read_exact(store, ctx, previous, 8 * 1024 * 1024)
        .map_err(|_| StorageError::ScopeDenied)?
        .0;
    let mut context: ProviderContext = serde_json::from_slice(&base).map_err(invalid)?;
    context.validate().map_err(|e| invalid(format!("{e:?}")))?;
    if context
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .any(|block| {
            matches!(block,
        ContentBlock::Native { binding: prior, .. } if prior != binding)
        })
    {
        return Err(invalid("foreign native state requires a new context epoch"));
    }
    let (entries, updates, observed_sequence) =
        read_snapshot(store, &ReadOptions::default(), |read| {
            let session = read
                .sessions()
                .session(session_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let attempt = read
                .usage()
                .inference_attempt(&attempt_id.0)?
                .ok_or(StorageError::ScopeDenied)?;
            if session.closed()
                || session.context_epoch != binding.context_epoch.get()
                || attempt.session_id.as_ref() != Some(session_id)
                || attempt.connection_id != binding.connection_id
                || attempt.catalog_model_id != binding.model_id
                || attempt.endpoint_profile != format!("{:?}", binding.endpoint_profile)
            {
                return Err(StorageError::ScopeDenied);
            }
            let mut position = after_sequence;
            let mut selected = Vec::new();
            let mut updates = Vec::new();
            while position < session.last_durable_sequence() {
                if position - after_sequence >= 10_000 {
                    return Err(invalid("tool turn exceeds transcript scan bound"));
                }
                let page = read.sessions().entries(session_id, position, 500)?;
                if page.is_empty() {
                    return Err(invalid("tool turn transcript gap"));
                }
                for entry in page {
                    if entry.sequence != position + 1 {
                        return Err(invalid("tool turn transcript gap"));
                    }
                    position = entry.sequence;
                    let body: TranscriptBody =
                        serde_json::from_value(entry.body).map_err(invalid)?;
                    if matches!(
                        &body,
                        TranscriptBody::UserMessage { .. } | TranscriptBody::HostNotice { .. }
                    ) {
                        return Err(StorageError::unavailable(
                            Unavailable::UnsupportedRuntime,
                            "tool continuation needs redirected context reconstruction",
                        ));
                    }
                    let belongs = match &body {
                        TranscriptBody::AssistantAttempt { attempt_id: id, .. } => id == attempt_id,
                        TranscriptBody::ToolCallRecorded { tool_call } => {
                            &tool_call.call.attempt_id == attempt_id
                        }
                        TranscriptBody::ToolDispatchStarted { call }
                        | TranscriptBody::ToolResponseRecorded { call, .. } => {
                            &call.attempt_id == attempt_id
                        }
                        TranscriptBody::OperationUpdate { .. } => true,
                        TranscriptBody::NativeItem { artifact, .. } => {
                            let row = read
                                .artifacts()
                                .artifact(&artifact.artifact_id)?
                                .ok_or(StorageError::ScopeDenied)?;
                            row.provenance["derived"]["attemptId"] == attempt_id.0.as_str()
                        }
                        _ => false,
                    };
                    if !belongs {
                        return Err(StorageError::unavailable(
                            Unavailable::UnsupportedRuntime,
                            "tool continuation is stale after another provider turn",
                        ));
                    }
                    if matches!(
                        &body,
                        TranscriptBody::AssistantAttempt { .. }
                            | TranscriptBody::ToolCallRecorded { .. }
                            | TranscriptBody::NativeItem { .. }
                    ) {
                        selected.push(body);
                    } else if let TranscriptBody::OperationUpdate {
                        operation_id,
                        revision,
                        artifact,
                    } = body
                    {
                        updates.push((operation_id, revision, artifact));
                        if updates.len() > 128 {
                            return Err(invalid("too many operation updates in turn continuation"));
                        }
                    }
                }
            }
            Ok((selected, updates, session.last_durable_sequence()))
        })?;
    let mut assistant = Vec::new();
    for entry in entries {
        match entry {
            TranscriptBody::AssistantAttempt {
                visible_text,
                interrupted,
                ..
            } => {
                if interrupted {
                    return Err(invalid("interrupted assistant tool turn"));
                }
                if !visible_text.is_empty() {
                    assistant.push(ContentBlock::Text {
                        text: visible_text,
                        cache: None,
                    });
                }
            }
            TranscriptBody::NativeItem {
                binding: native_binding,
                artifact,
            } => {
                if &native_binding != binding {
                    return Err(invalid("foreign native item in tool turn"));
                }
                assistant.push(ContentBlock::Native {
                    binding: native_binding,
                    artifact,
                });
            }
            TranscriptBody::ToolCallRecorded { tool_call } => {
                let bytes = artifacts
                    .read_exact(store, ctx, &tool_call.native_item, 1024 * 1024)
                    .map_err(|_| StorageError::ScopeDenied)?
                    .0;
                let native: Value = serde_json::from_slice(&bytes).map_err(invalid)?;
                let wire_name = native
                    .get("name")
                    .or_else(|| native.get("function").and_then(|v| v.get("name")))
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty() && name.len() <= 128)
                    .ok_or_else(|| invalid("recorded native tool alias missing"))?;
                assistant.push(ContentBlock::ToolCall {
                    call_id: tool_call.call.native_call_id,
                    wire_name: wire_name.into(),
                    arguments: tool_call.arguments,
                });
            }
            _ => return Err(invalid("unexpected selected tool turn entry")),
        }
    }
    if assistant.is_empty() && has_tools {
        return Err(invalid("empty assistant tool turn"));
    }
    let mut results = Vec::new();
    for item in resolved {
        let (content, is_error) = match item.outcome {
            ToolOutcome::Completed { artifacts: refs }
            | ToolOutcome::OperationAccepted {
                artifacts: refs, ..
            } => {
                let mut blocks = Vec::with_capacity(refs.len());
                for reference in refs {
                    if reference.media_type.starts_with("image/") {
                        blocks.push(ResultBlock::Image {
                            artifact: reference,
                            detail: None,
                        });
                    } else if reference.media_type.starts_with("text/")
                        || reference.media_type == "application/json"
                    {
                        let bytes = artifacts
                            .read_exact(store, ctx, &reference, 8 * 1024 * 1024)
                            .map_err(|_| StorageError::ScopeDenied)?
                            .0;
                        blocks.push(ResultBlock::Text {
                            text: String::from_utf8(bytes).map_err(invalid)?,
                        });
                    } else {
                        return Err(invalid("unsupported tool result artifact media type"));
                    }
                }
                (blocks, false)
            }
            other => (
                vec![ResultBlock::Text {
                    text: serde_json::to_string(&other).map_err(invalid)?,
                }],
                true,
            ),
        };
        results.push(ContentBlock::ToolResult {
            call_id: item.call.call.native_call_id,
            content,
            is_error,
            cache: None,
        });
    }
    if !assistant.is_empty() {
        context.messages.push(Message {
            role: MessageRole::Assistant,
            content: assistant,
        });
    }
    if !results.is_empty() {
        context.messages.push(Message {
            role: MessageRole::User,
            content: results,
        });
    }
    for (operation_id, revision, artifact) in updates {
        context.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: operation_update_text(
                    store,
                    artifacts,
                    ctx,
                    session_id,
                    &operation_id,
                    &revision,
                    &artifact,
                )?,
                cache: None,
            }],
        });
    }
    context.validate().map_err(|e| invalid(format!("{e:?}")))?;
    Ok(ContinuationMaterial {
        context,
        observed_sequence,
    })
}

/// Publish the settled exchange as the next immutable request context. A replay may publish the
/// same bytes again, but it cannot repeat a tool effect or rewrite the original transcript.
pub(crate) fn publish_continuation(
    store: &dyn ApplicationStore,
    artifacts: &Arc<ArtifactService>,
    ctx: &InvocationContext,
    session_id: &Id,
    attempt_id: &AttemptId,
    binding: &NativeBinding,
    material: ContinuationMaterial,
) -> Result<devworks_core::protocol::provider::ArtifactRef, StorageError> {
    let ContinuationMaterial {
        context,
        observed_sequence,
    } = material;
    context
        .validate()
        .map_err(|error| invalid(format!("tool continuation: {error:?}")))?;
    let bytes = serde_json::to_vec(&context).map_err(invalid)?;
    let staged = artifacts
        .stage_host_bytes(
            store,
            &bytes,
            json!({"kind":"ProviderTurnContinuation","sessionId":session_id,
                "attemptId":attempt_id,"binding":binding,"observedSequence":observed_sequence}),
        )?
        .map_err(invalid)?;
    let staged_for_write = staged.clone();
    let service = artifacts.clone();
    let ctx = ctx.clone();
    let session_id = session_id.clone();
    let attempt_id = attempt_id.clone();
    let binding = binding.clone();
    let committed = run_write(store, WriteOptions::default(), move |tx| {
        let session = tx
            .sessions()
            .session(&session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let attempt = tx
            .usage()
            .inference_attempt(&attempt_id.0)?
            .ok_or(StorageError::ScopeDenied)?;
        if session.closed()
            || session.context_epoch != binding.context_epoch.get()
            || session.last_durable_sequence() != observed_sequence
            || attempt.session_id.as_ref() != Some(&session_id)
            || attempt.connection_id != binding.connection_id
            || attempt.catalog_model_id != binding.model_id
            || attempt.endpoint_profile != format!("{:?}", binding.endpoint_profile)
            || ctx.session_id() != Some(&session_id)
            || tx.store().store_id != *ctx.store_id()
            || tx.store().restore_epoch != ctx.restore_epoch().get()
            || tx.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
            || !matches!(
                ctx.principal_kind(),
                PrincipalKind::Host | PrincipalKind::Agent
            )
        {
            return Err(StorageError::ScopeDenied);
        }
        if ctx.principal_kind() == PrincipalKind::Agent {
            let principal = tx
                .authority()
                .principal(ctx.principal())?
                .ok_or(StorageError::ScopeDenied)?;
            if !principal.active
                || session.agent_id != *ctx.principal()
                || principal
                    .project_id
                    .as_ref()
                    .is_some_and(|project| session.project_id.as_ref() != Some(project))
            {
                return Err(StorageError::ScopeDenied);
            }
        }
        let scope = session
            .project_id
            .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
        let version = service.commit_host_bytes_in_scope(
            tx,
            &staged_for_write,
            HostArtifactPublication {
                title: "Provider turn continuation",
                media_type: "application/vnd.devworks.provider-context+json",
                provenance: json!({"kind":"ProviderTurnContinuation","sessionId":session_id,
                    "attemptId":attempt_id,"binding":binding,"observedSequence":observed_sequence}),
                scope,
                owner: ctx.principal().clone(),
            },
        )?;
        Ok(version_ref(&version))
    });
    artifacts.settle_host_bytes(store, &staged, committed.is_ok());
    committed
}

fn operation_update_text(
    store: &dyn ApplicationStore,
    artifacts: &Arc<ArtifactService>,
    ctx: &InvocationContext,
    session_id: &Id,
    operation_id: &OperationId,
    revision: &Revision,
    artifact: &ArtifactRef,
) -> Result<String, StorageError> {
    let version = read_snapshot(store, &ReadOptions::default(), |read| {
        read.artifacts()
            .version(&artifact.version_id)?
            .ok_or(StorageError::ScopeDenied)
    })?;
    if !version.live()
        || version.artifact_id != artifact.artifact_id
        || version.source["principalKind"] != "Host"
        || version.source["producedBy"] != store.info().host_actor.0.as_str()
    {
        return Err(StorageError::ScopeDenied);
    }
    let (bytes, provenance) = artifacts
        .read_exact(store, ctx, artifact, 1024 * 1024)
        .map_err(|_| StorageError::ScopeDenied)?;
    if provenance["principalKind"] != "Host"
        || provenance["derived"]["kind"] != "ToolOperationUpdate"
        || provenance["derived"]["sessionId"] != session_id.as_str()
        || provenance["derived"]["operationId"] != operation_id.0.as_str()
        || provenance["derived"]["revision"] != revision.get()
    {
        return Err(StorageError::ScopeDenied);
    }
    let payload: Value = serde_json::from_slice(&bytes).map_err(invalid)?;
    if payload["operationId"] != operation_id.0.as_str() || payload["revision"] != revision.get() {
        return Err(StorageError::ScopeDenied);
    }
    Ok(format!(
        "Tool operation update: {}",
        String::from_utf8(bytes).map_err(invalid)?
    ))
}

/// Reconcile later operation facts into an append-only provider context. A provider's first tool
/// result remains immutable; each subsequent journal revision enters as a separate host-labeled
/// message. Any other transcript entry requires the caller to rebuild redirected context.
pub(crate) fn append_operation_updates(
    store: &dyn ApplicationStore,
    artifacts: &Arc<ArtifactService>,
    ctx: &InvocationContext,
    session_id: &Id,
    source: &ArtifactRef,
) -> Result<ArtifactRef, StorageError> {
    let (bytes, provenance) = artifacts
        .read_exact(store, ctx, source, 8 * 1024 * 1024)
        .map_err(|_| StorageError::ScopeDenied)?;
    let derived = &provenance["derived"];
    if provenance["principalKind"] != "Host"
        || provenance["producedBy"] != store.info().host_actor.0.as_str()
        || derived["sessionId"] != session_id.as_str()
        || !matches!(
            derived["kind"].as_str(),
            Some(
                "ProviderTurnContinuation"
                    | "ProviderToolContinuation"
                    | "ProviderOperationUpdateContext"
                    | "ProviderContextEpoch"
            )
        )
    {
        return Err(StorageError::ScopeDenied);
    }
    let binding: NativeBinding =
        serde_json::from_value(derived["binding"].clone()).map_err(invalid)?;
    let after = derived["observedSequence"]
        .as_i64()
        .ok_or_else(|| invalid("context source lacks transcript cursor"))?;
    let mut context: ProviderContext = serde_json::from_slice(&bytes).map_err(invalid)?;
    context
        .validate()
        .map_err(|error| invalid(format!("{error:?}")))?;
    let (updates, observed) = read_snapshot(store, &ReadOptions::default(), |read| {
        let session = authorized_session(read, ctx, session_id)?;
        let version = read
            .artifacts()
            .version(&source.version_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if session.context_epoch != binding.context_epoch.get()
            || after < 0
            || after > session.last_durable_sequence()
            || !version.live()
            || version.artifact_id != source.artifact_id
            || version.source["principalKind"] != "Host"
            || version.source["producedBy"] != store.info().host_actor.0.as_str()
        {
            return Err(StorageError::ScopeDenied);
        }
        let mut position = after;
        let mut updates = Vec::new();
        while position < session.last_durable_sequence() {
            if position - after >= 10_000 {
                return Err(invalid(
                    "operation update context exceeds transcript scan bound",
                ));
            }
            let page = read.sessions().entries(session_id, position, 500)?;
            if page.is_empty() {
                return Err(invalid("operation update transcript gap"));
            }
            for entry in page {
                if entry.sequence != position + 1 {
                    return Err(invalid("operation update transcript gap"));
                }
                position = entry.sequence;
                let TranscriptBody::OperationUpdate {
                    operation_id,
                    revision,
                    artifact,
                } = serde_json::from_value(entry.body).map_err(invalid)?
                else {
                    return Err(StorageError::unavailable(
                        Unavailable::UnsupportedRuntime,
                        "tool context needs redirected transcript reconstruction",
                    ));
                };
                updates.push((operation_id, revision, artifact));
                if updates.len() > 128 {
                    return Err(invalid(
                        "too many operation updates for one context revision",
                    ));
                }
            }
        }
        Ok((updates, position))
    })?;
    if updates.is_empty() {
        return Ok(source.clone());
    }
    let mut blocks = Vec::with_capacity(updates.len());
    for (operation_id, revision, artifact) in updates {
        blocks.push(ContentBlock::Text {
            text: operation_update_text(
                store,
                artifacts,
                ctx,
                session_id,
                &operation_id,
                &revision,
                &artifact,
            )?,
            cache: None,
        });
    }
    context.messages.push(Message {
        role: MessageRole::User,
        content: blocks,
    });
    context
        .validate()
        .map_err(|error| invalid(format!("{error:?}")))?;
    let bytes = serde_json::to_vec(&context).map_err(invalid)?;
    let staged = artifacts
        .stage_host_bytes(
            store,
            &bytes,
            json!({"kind":"ProviderOperationUpdateContext",
        "sessionId":session_id,"binding":binding,"observedSequence":observed}),
        )?
        .map_err(invalid)?;
    let staged_for_write = staged.clone();
    let artifacts_for_write = artifacts.clone();
    let ctx_for_write = ctx.clone();
    let session_for_write = session_id.clone();
    let binding_for_write = binding.clone();
    let committed = run_write(store, WriteOptions::default(), move |tx| {
        let session = tx
            .sessions()
            .session(&session_for_write)?
            .ok_or(StorageError::ScopeDenied)?;
        if session.context_epoch != binding_for_write.context_epoch.get()
            || session.last_durable_sequence() != observed
            || ctx_for_write.session_id() != Some(&session_for_write)
            || tx.store().store_id != *ctx_for_write.store_id()
            || tx.store().restore_epoch != ctx_for_write.restore_epoch().get()
            || tx.actors().actor_kind(ctx_for_write.principal())?
                != Some(ctx_for_write.principal_kind())
            || !matches!(
                ctx_for_write.principal_kind(),
                PrincipalKind::Host | PrincipalKind::Agent
            )
            || session.closed()
        {
            return Err(StorageError::ScopeDenied);
        }
        if ctx_for_write.principal_kind() == PrincipalKind::Agent {
            let principal = tx
                .authority()
                .principal(ctx_for_write.principal())?
                .ok_or(StorageError::ScopeDenied)?;
            if !principal.active
                || session.agent_id != *ctx_for_write.principal()
                || principal
                    .project_id
                    .as_ref()
                    .is_some_and(|project| session.project_id.as_ref() != Some(project))
            {
                return Err(StorageError::ScopeDenied);
            }
        }
        let scope = session
            .project_id
            .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
        let version = artifacts_for_write.commit_host_bytes_in_scope(tx, &staged_for_write,
            HostArtifactPublication { title: "Provider operation update context",
                media_type: "application/vnd.devworks.provider-context+json",
                provenance: json!({"kind":"ProviderOperationUpdateContext","sessionId":session_for_write,
                    "binding":binding_for_write,"observedSequence":observed}),
                scope, owner: ctx_for_write.principal().clone() })?;
        Ok(version_ref(&version))
    });
    artifacts.settle_host_bytes(store, &staged, committed.is_ok());
    committed
}

/// Build the exact frame sent to the application gateway for a recorded native call. The
/// stable key makes a retried command find its original receipt after a crash or lost reply.
pub(crate) fn frame(
    session: &Id,
    call: &ToolCallKey,
    descriptor: &'static OperationDescriptor,
    mode: Mode,
    revision: SchemaRevision,
    arguments: Value,
) -> Result<Vec<u8>, &'static str> {
    call.validate()?;
    if !descriptor.supports(revision) || descriptor.binding(mode).is_none() {
        return Err("unsupported selected tool binding");
    }
    let Value::Object(mut arguments) = arguments else {
        return Err("tool arguments must be an object");
    };
    let identity = format!(
        "tool-{}",
        sha256_hex(
            format!(
                "{}\0{}\0{}\0{}",
                session.as_str(),
                call.attempt_id.0.as_str(),
                call.native_call_id,
                call.ordinal.as_str()
            )
            .as_bytes()
        )
    );
    let key = mode.key_member();
    let value = match descriptor.envelope {
        Envelope::Domain => {
            // The discriminator is host selected. Accepting a provider-supplied one would let a
            // schema shared by multiple operations silently select another authority binding.
            if arguments.remove("kind").is_some() {
                return Err("provider supplied a domain selector");
            }
            arguments.insert("kind".into(), Value::String(descriptor.name.into()));
            let mut envelope = serde_json::Map::new();
            envelope.insert("protocol".into(), Value::String(CONTRACT.into()));
            envelope.insert("mode".into(), Value::String(mode.as_str().into()));
            envelope.insert(key.into(), Value::String(identity));
            envelope.insert("body".into(), Value::Object(arguments));
            envelope.insert("schemaRevision".into(), json!(revision.0));
            Value::Object(envelope)
        }
        Envelope::Specialized { .. } => {
            // Older specialized schemas describe the whole wire envelope. Treat any model
            // values in its identity members as data and replace them with the host binding.
            arguments.remove("protocol");
            arguments.remove(key);
            arguments.remove("schemaRevision");
            arguments.insert("protocol".into(), Value::String(CONTRACT.into()));
            arguments.insert(key.into(), Value::String(identity));
            arguments.insert("schemaRevision".into(), json!(revision.0));
            Value::Object(arguments)
        }
    };
    let bytes = serde_json::to_vec(&value).map_err(|_| "invalid tool frame")?;
    admission::decode(
        Selection {
            operation: descriptor.name,
            mode,
        },
        &bytes,
    )
    .map_err(|_| "tool arguments failed selected-operation admission")?;
    Ok(bytes)
}

/// Execute one durable, unanswered provider call through the same gateway used by UI and agent
/// tools. The call's recorded schema chooses the operation; provider arguments never choose it.
/// Command retries reuse the host key, so a lost reply cannot repeat the effect.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_one(
    runtime: &Runtime,
    store: &dyn ApplicationStore,
    artifacts: &Arc<ArtifactService>,
    schemas: &RegisteredSchemas,
    ctx: &InvocationContext,
    session_id: &Id,
    key: &ToolCallKey,
    cancellation: Option<&Cancellation>,
) -> Result<BatchOutcome, StorageError> {
    let (call, started, call_sequence, context_epoch) =
        read_snapshot(store, &ReadOptions::default(), |read| {
            let session = read
                .sessions()
                .session(session_id)?
                .ok_or(StorageError::ScopeDenied)?;
            if ctx.session_id() != Some(session_id)
                || session.closed()
                || read.stamp().store_id != *ctx.store_id()
                || read.stamp().restore_epoch != ctx.restore_epoch().get()
                || !matches!(
                    ctx.principal_kind(),
                    PrincipalKind::Agent | PrincipalKind::Host
                )
                || read.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
            {
                return Err(StorageError::ScopeDenied);
            }
            if ctx.principal_kind() == PrincipalKind::Agent {
                let binding = read
                    .authority()
                    .principal(ctx.principal())?
                    .ok_or(StorageError::ScopeDenied)?;
                if !binding.active
                    || session.agent_id != *ctx.principal()
                    || binding
                        .project_id
                        .as_ref()
                        .is_some_and(|p| session.project_id.as_ref() != Some(p))
                {
                    return Err(StorageError::ScopeDenied);
                }
            }
            let row = read
                .sessions()
                .tool_call(session_id, &key.attempt_id.0, &key.native_call_id)?
                .ok_or(StorageError::ScopeDenied)?;
            if row.ordinal != key.ordinal.get() || row.response_sequence.is_some() {
                return Err(StorageError::ScopeDenied);
            }
            let entry = read
                .sessions()
                .entries(session_id, row.call_sequence - 1, 1)?
                .into_iter()
                .next()
                .filter(|e| e.sequence == row.call_sequence)
                .ok_or(StorageError::ScopeDenied)?;
            let TranscriptBody::ToolCallRecorded { tool_call } =
                serde_json::from_value(entry.body).map_err(|_| StorageError::ScopeDenied)?
            else {
                return Err(StorageError::ScopeDenied);
            };
            if tool_call.call.attempt_id != key.attempt_id
                || tool_call.call.native_call_id != key.native_call_id
                || tool_call.call.ordinal != key.ordinal
                || tool_call.capability_id.as_str() != row.capability_id
            {
                return Err(StorageError::ScopeDenied);
            }
            let started = devworks_core::services::sessions::transcript::tool_dispatch_started(
                read.sessions(),
                session_id,
                key,
                row.call_sequence,
                session.last_durable_sequence(),
            )?;
            Ok((tool_call, started, row.call_sequence, session.context_epoch))
        })?;

    if !started {
        let (source, targets) = read_snapshot(store, &ReadOptions::default(), |read| {
            let attempt = read
                .usage()
                .inference_attempt(&key.attempt_id.0)?
                .ok_or(StorageError::ScopeDenied)?;
            if attempt.session_id.as_ref() != Some(session_id) || attempt.state != "Settled" {
                return Err(StorageError::ScopeDenied);
            }
            let page = read.history().page(
                &EventFilter::Aggregate {
                    kind: "provider_result".into(),
                    id: key.attempt_id.0.as_str().into(),
                },
                0,
                1,
            )?;
            let event = page.events.first().ok_or_else(|| {
                StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    "tool dispatch waits for settled provider provenance",
                )
            })?;
            if event.kind != "ProviderResultRecorded"
                || event.payload["attemptId"] != json!(key.attempt_id)
                || event.payload["outcome"] != "ToolCallsReady"
                || event.payload["turnStartSequence"]
                    .as_i64()
                    .is_none_or(|start| start < 0 || start >= call_sequence)
            {
                return Err(StorageError::ScopeDenied);
            }
            let binding: NativeBinding = serde_json::from_value(event.payload["binding"].clone())
                .map_err(|_| StorageError::ScopeDenied)?;
            if binding.connection_id != attempt.connection_id
                || binding.model_id != attempt.catalog_model_id
                || format!("{:?}", binding.endpoint_profile) != attempt.endpoint_profile
                || binding.context_epoch.get() != context_epoch
            {
                return Err(StorageError::ScopeDenied);
            }
            let source: ArtifactRef =
                serde_json::from_value(event.payload["sourceContext"].clone()).map_err(|_| {
                    StorageError::unavailable(
                        Unavailable::UnsupportedRuntime,
                        "tool dispatch needs recorded instruction context",
                    )
                })?;
            let targets: Option<Vec<(Id, Vec<String>)>> =
                serde_json::from_value(event.payload["instructionTargets"].clone())
                    .map_err(|_| StorageError::ScopeDenied)?;
            Ok((source, targets))
        })?;
        let providers = runtime.providers().ok_or_else(|| {
            StorageError::unavailable(
                Unavailable::UnsupportedRuntime,
                "native provider unavailable",
            )
        })?;
        providers.verify_instruction_material(ctx, &source, targets.as_deref())?;
    }

    if cancellation.is_some_and(Cancellation::is_cancelled) {
        return Err(StorageError::Cancelled);
    }
    let result = match selected_builtin(schemas, &call) {
        Ok((descriptor, mode, revision)) => {
            // Missing or unreadable input material is a recovery hold. It must not become a
            // permanent Failed response just because the local artifact read is unavailable.
            let bytes = artifacts
                .read_exact(store, ctx, &call.arguments, 8 * 1024 * 1024)
                .map_err(|_| StorageError::ScopeDenied)?
                .0;
            match devworks_core::framing::parse(&bytes) {
                Ok(args) => frame(session_id, key, descriptor, mode, revision, args)
                    .map(|raw| (descriptor, mode, raw)),
                Err(_) => Err("invalid tool arguments"),
            }
        }
        // A recorded alias whose implementation is not composed here (for example a package
        // tool) is not a provider argument error. Leave it unanswered for a qualified dispatcher
        // rather than permanently closing the call with a false failure.
        Err(_) => {
            return Err(StorageError::unavailable(
                Unavailable::UnsupportedRuntime,
                "recorded tool binding unavailable",
            ));
        }
    };
    if cancellation.is_some_and(Cancellation::is_cancelled) {
        return Err(StorageError::Cancelled);
    }
    if result.is_ok() {
        mark_dispatch_started(store, ctx, session_id, key, cancellation)?;
        if cancellation.is_some_and(Cancellation::is_cancelled) {
            return Err(StorageError::Cancelled);
        }
    }
    let (outcome, bytes) = match result {
        Ok((descriptor, Mode::Query, raw)) => match runtime.query(ctx, descriptor.name, &raw) {
            Ok(reply @ DomainQueryReply::Result { .. }) => {
                completed(serde_json::to_vec(&reply).map_err(invalid)?)
            }
            Ok(DomainQueryReply::Rejected { fault, .. }) => {
                if matches!(
                    fault.retry,
                    RetryAdvice::SameCommand
                        | RetryAdvice::ResolveReceipt
                        | RetryAdvice::ReconcileOperation
                ) {
                    return Err(StorageError::unavailable(
                        Unavailable::UnsupportedRuntime,
                        "tool query requires retry or reconciliation",
                    ));
                }
                failed(&format!("domain_{:?}", fault.code))
            }
            Err(_) => {
                return Err(StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    "tool query gateway failed before a durable response",
                ));
            }
        },
        Ok((descriptor, Mode::Command, raw)) => match runtime.command(ctx, descriptor.name, &raw) {
            Ok(CommandReply::CommandStatusUnknown { .. }) => {
                // Same host key is retried on recovery until the gateway returns its receipt.
                return Err(StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    "tool command status unknown",
                ));
            }
            Ok(CommandReply::ReceiptPurged { .. }) => {
                return Err(StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    "tool command receipt purged before outcome reconciliation",
                ));
            }
            Ok(reply @ CommandReply::Recorded { .. }) => recorded_command(&reply)?,
            Ok(CommandReply::Rejected { code, retry, .. }) => {
                if matches!(
                    retry,
                    RetryAdvice::SameCommand
                        | RetryAdvice::ResolveReceipt
                        | RetryAdvice::ReconcileOperation
                ) {
                    return Err(StorageError::unavailable(
                        Unavailable::UnsupportedRuntime,
                        "tool command rejection requires reconciliation",
                    ));
                }
                failed(&format!("command_{code:?}"))
            }
            Err(_) => {
                return Err(StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    "tool command gateway failed before receipt reconciliation",
                ));
            }
        },
        Err(_) => failed("tool_invalid_arguments"),
    };
    let staged =
        if let Some(bytes) = &bytes {
            Some(artifacts.stage_host_bytes(store, bytes,
            json!({"kind":"ToolResult","sessionId":session_id,"attemptId":key.attempt_id,
                "nativeCallId":key.native_call_id}))?
            .map_err(invalid)?)
        } else {
            None
        };
    let staged_for_write = staged.clone();
    let artifacts_for_write = artifacts.clone();
    let ctx_for_write = ctx.clone();
    let session_for_write = session_id.clone();
    let key_for_write = key.clone();
    let committed = run_write(store, WriteOptions::default(), move |tx| {
        let mut outcome = outcome.clone();
        if let Some(staged) = &staged_for_write {
            let session = tx
                .sessions()
                .session(&session_for_write)?
                .ok_or(StorageError::ScopeDenied)?;
            let scope = session
                .project_id
                .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
            let version = artifacts_for_write.commit_host_bytes_in_scope(tx, staged, HostArtifactPublication {
                title: "Tool result", media_type: "application/json",
                provenance: json!({"kind":"ToolResult","sessionId":session_for_write,"call":key_for_write}),
                scope, owner: ctx_for_write.principal().clone(),
            })?;
            match &mut outcome {
                ToolOutcome::Completed { artifacts }
                | ToolOutcome::OperationAccepted { artifacts, .. } => {
                    *artifacts = vec![version_ref(&version)];
                }
                _ => return Err(invalid("tool result artifact without successful outcome")),
            }
        }
        let session = tx
            .sessions()
            .session(&session_for_write)?
            .ok_or(StorageError::ScopeDenied)?;
        let batch = turn::batch_of(
            &session_for_write,
            tx.new_id(IdKind::TranscriptBatch),
            session.context_epoch,
            session.next_sequence,
            vec![TranscriptBody::ToolResponseRecorded {
                call: key_for_write.clone(),
                outcome,
            }],
            || tx.new_id(IdKind::TranscriptEntry),
        );
        match SessionWriter::append_in(tx, &ctx_for_write, &SessionBatch::entries(batch))? {
            accepted @ (BatchOutcome::Accepted(_) | BatchOutcome::Replayed(_)) => Ok(accepted),
            BatchOutcome::Refused(_) => Err(StorageError::ScopeDenied),
        }
    });
    if let Some(staged) = &staged {
        artifacts.settle_host_bytes(store, staged, committed.is_ok());
    }
    committed
}

fn mark_dispatch_started(
    store: &dyn ApplicationStore,
    ctx: &InvocationContext,
    session_id: &Id,
    key: &ToolCallKey,
    cancellation: Option<&Cancellation>,
) -> Result<(), StorageError> {
    let ctx = ctx.clone();
    let session_id = session_id.clone();
    let key = key.clone();
    let cancellation = cancellation.cloned();
    run_write(store, WriteOptions::default(), move |tx| {
        if cancellation
            .as_ref()
            .is_some_and(Cancellation::is_cancelled)
        {
            return Err(StorageError::Cancelled);
        }
        let session = tx
            .sessions()
            .session(&session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if session.closed()
            || ctx.session_id() != Some(&session_id)
            || tx.store().store_id != *ctx.store_id()
            || tx.store().restore_epoch != ctx.restore_epoch().get()
            || !matches!(
                ctx.principal_kind(),
                PrincipalKind::Agent | PrincipalKind::Host
            )
            || tx.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
        {
            return Err(StorageError::ScopeDenied);
        }
        if ctx.principal_kind() == PrincipalKind::Agent {
            let binding = tx
                .authority()
                .principal(ctx.principal())?
                .ok_or(StorageError::ScopeDenied)?;
            if !binding.active
                || session.agent_id != *ctx.principal()
                || binding
                    .project_id
                    .as_ref()
                    .is_some_and(|project| session.project_id.as_ref() != Some(project))
            {
                return Err(StorageError::ScopeDenied);
            }
        }
        let row = tx
            .sessions()
            .tool_call(&session_id, &key.attempt_id.0, &key.native_call_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if row.ordinal != key.ordinal.get() || row.response_sequence.is_some() {
            return Err(StorageError::ScopeDenied);
        }
        if devworks_core::services::sessions::transcript::tool_dispatch_started(
            &*tx.sessions(),
            &session_id,
            &key,
            row.call_sequence,
            session.last_durable_sequence(),
        )? {
            return Ok(());
        }
        let batch = turn::batch_of(
            &session_id,
            tx.new_id(IdKind::TranscriptBatch),
            session.context_epoch,
            session.next_sequence,
            vec![TranscriptBody::ToolDispatchStarted { call: key }],
            || tx.new_id(IdKind::TranscriptEntry),
        );
        match SessionWriter::append_in(tx, &ctx, &SessionBatch::entries(batch))? {
            BatchOutcome::Accepted(_) => Ok(()),
            BatchOutcome::Replayed(_) => Err(invalid("tool dispatch marker unexpectedly replayed")),
            BatchOutcome::Refused(_) => Err(StorageError::ScopeDenied),
        }
    })
}

/// A cancellation is final only when its response commits before the dispatch marker. Once
/// started, a same-key recovery or explicit effect reconciliation must settle the call.
pub(crate) fn cancel_before_dispatch(
    store: &dyn ApplicationStore,
    ctx: &InvocationContext,
    session_id: &Id,
    key: &ToolCallKey,
) -> Result<BatchOutcome, StorageError> {
    let ctx = ctx.clone();
    let session_id = session_id.clone();
    let key = key.clone();
    run_write(store, WriteOptions::default(), move |tx| {
        let session = tx
            .sessions()
            .session(&session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if session.closed() {
            return Err(StorageError::ScopeDenied);
        }
        let batch = turn::batch_of(
            &session_id,
            tx.new_id(IdKind::TranscriptBatch),
            session.context_epoch,
            session.next_sequence,
            vec![TranscriptBody::ToolResponseRecorded {
                call: key,
                outcome: ToolOutcome::CancelledBeforeDispatch,
            }],
            || tx.new_id(IdKind::TranscriptEntry),
        );
        match SessionWriter::append_in(tx, &ctx, &SessionBatch::entries(batch))? {
            accepted @ BatchOutcome::Accepted(_) => Ok(accepted),
            BatchOutcome::Refused(
                devworks_core::services::sessions::transcript::BatchRefusal::DispatchStarted,
            ) => Err(StorageError::unavailable(
                Unavailable::UnsupportedRuntime,
                "tool dispatch already started; reconcile effect before cancellation",
            )),
            BatchOutcome::Replayed(_) | BatchOutcome::Refused(_) => Err(StorageError::ScopeDenied),
        }
    })
}

/// Append a later operation state as a separate fact. The accepted tool receipt and its first
/// response stay immutable; repeated recovery of the same journal revision is a no-op.
pub(crate) fn record_operation_update(
    store: &dyn ApplicationStore,
    artifacts: &Arc<ArtifactService>,
    ctx: &InvocationContext,
    session_id: &Id,
    operation_id: &OperationId,
) -> Result<Option<devworks_core::protocol::provider::ArtifactRef>, StorageError> {
    if ctx.session_id() != Some(session_id) {
        return Err(StorageError::ScopeDenied);
    }
    // Apply the same current session gate even when the requested revision was already
    // published and this call will return without appending a batch.
    let observed = read_snapshot(store, &ReadOptions::default(), |read| {
        authorized_session(read, ctx, session_id)?;
        let linked = read.sessions().linked_operations(session_id, 500)?;
        if linked.len() == 500 {
            return Err(invalid("linked tool operations exceed scan bound"));
        }
        if !linked
            .iter()
            .any(|call| call.operation_id.as_ref() == Some(operation_id))
        {
            return Err(StorageError::ScopeDenied);
        }
        let operation = read
            .operations()
            .operation(operation_id)?
            .ok_or(StorageError::ScopeDenied)?;
        Ok((operation.state, operation.revision))
    })?;
    let bytes = serde_json::to_vec(&json!({
        "operationId": operation_id,
        "revision": observed.1,
        "state": observed.0,
    }))
    .map_err(invalid)?;
    let staged = artifacts
        .stage_host_bytes(
            store,
            &bytes,
            json!({"kind":"ToolOperationUpdate","sessionId":session_id,
                "operationId":operation_id,"revision":observed.1}),
        )?
        .map_err(invalid)?;
    let staged_for_write = staged.clone();
    let artifacts_for_write = artifacts.clone();
    let ctx_for_write = ctx.clone();
    let session_for_write = session_id.clone();
    let operation_for_write = operation_id.clone();
    let committed = run_write(store, WriteOptions::default(), move |tx| {
        let session = tx
            .sessions()
            .session(&session_for_write)?
            .ok_or(StorageError::ScopeDenied)?;
        if session.closed() {
            return Err(StorageError::ScopeDenied);
        }
        if tx.store().store_id != *ctx_for_write.store_id()
            || tx.store().restore_epoch != ctx_for_write.restore_epoch().get()
            || tx.actors().actor_kind(ctx_for_write.principal())?
                != Some(ctx_for_write.principal_kind())
            || !matches!(
                ctx_for_write.principal_kind(),
                PrincipalKind::Host | PrincipalKind::Agent
            )
        {
            return Err(StorageError::ScopeDenied);
        }
        if ctx_for_write.principal_kind() == PrincipalKind::Agent {
            let binding = tx
                .authority()
                .principal(ctx_for_write.principal())?
                .ok_or(StorageError::ScopeDenied)?;
            if !binding.active
                || session.agent_id != *ctx_for_write.principal()
                || binding
                    .project_id
                    .as_ref()
                    .is_some_and(|project| session.project_id.as_ref() != Some(project))
            {
                return Err(StorageError::ScopeDenied);
            }
        }
        let linked = tx.sessions().linked_operations(&session_for_write, 500)?;
        if linked.len() == 500 {
            return Err(invalid("linked tool operations exceed scan bound"));
        }
        let first_response = linked
            .iter()
            .filter(|call| call.operation_id.as_ref() == Some(&operation_for_write))
            .filter_map(|call| call.response_sequence)
            .min()
            .ok_or(StorageError::ScopeDenied)?;
        let operation = tx
            .operations()
            .operation(&operation_for_write)?
            .ok_or(StorageError::ScopeDenied)?;
        if (operation.state, operation.revision) != observed {
            return Err(StorageError::unavailable(
                Unavailable::UnsupportedRuntime,
                "tool operation changed during update publication",
            ));
        }
        let mut position = first_response;
        while position < session.last_durable_sequence() {
            if position - first_response >= 10_000 {
                return Err(invalid("operation update transcript exceeds scan bound"));
            }
            let page = tx.sessions().entries(&session_for_write, position, 500)?;
            if page.is_empty() {
                return Err(invalid("operation update transcript gap"));
            }
            for entry in page {
                if entry.sequence != position + 1 {
                    return Err(invalid("operation update transcript gap"));
                }
                position = entry.sequence;
                if entry.kind != "OperationUpdate" {
                    continue;
                }
                let TranscriptBody::OperationUpdate {
                    operation_id,
                    revision,
                    ..
                } = serde_json::from_value(entry.body).map_err(invalid)?
                else {
                    return Err(invalid("operation update kind mismatch"));
                };
                if operation_id == operation_for_write && revision.get() >= observed.1 {
                    return if revision.get() == observed.1 {
                        Ok(None)
                    } else {
                        Err(invalid("operation update revision regressed"))
                    };
                }
            }
        }
        let scope = session
            .project_id
            .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
        let version = artifacts_for_write.commit_host_bytes_in_scope(
            tx,
            &staged_for_write,
            HostArtifactPublication {
                title: "Tool operation update",
                media_type: "application/json",
                provenance: json!({"kind":"ToolOperationUpdate","sessionId":session_for_write,
                    "operationId":operation_for_write,"revision":observed.1}),
                scope,
                owner: ctx_for_write.principal().clone(),
            },
        )?;
        let reference = version_ref(&version);
        let batch = turn::batch_of(
            &session_for_write,
            tx.new_id(IdKind::TranscriptBatch),
            session.context_epoch,
            session.next_sequence,
            vec![TranscriptBody::OperationUpdate {
                operation_id: operation_for_write.clone(),
                revision: devworks_core::protocol::Revision::new(observed.1).map_err(invalid)?,
                artifact: reference.clone(),
            }],
            || tx.new_id(IdKind::TranscriptEntry),
        );
        match SessionWriter::append_in(tx, &ctx_for_write, &SessionBatch::entries(batch))? {
            BatchOutcome::Accepted(_) => Ok(Some(reference)),
            BatchOutcome::Replayed(_) => Err(invalid("operation update unexpectedly replayed")),
            BatchOutcome::Refused(_) => Err(StorageError::ScopeDenied),
        }
    });
    artifacts.settle_host_bytes(store, &staged, matches!(&committed, Ok(Some(_))));
    committed
}

fn completed(bytes: Vec<u8>) -> (ToolOutcome, Option<Vec<u8>>) {
    (ToolOutcome::Completed { artifacts: vec![] }, Some(bytes))
}

fn accepted(bytes: Vec<u8>, operation_id: OperationId) -> (ToolOutcome, Option<Vec<u8>>) {
    (
        ToolOutcome::OperationAccepted {
            operation_id,
            artifacts: vec![],
        },
        Some(bytes),
    )
}

fn recorded_command(
    reply: &DomainCommandReply,
) -> Result<(ToolOutcome, Option<Vec<u8>>), StorageError> {
    let CommandReply::Recorded { receipt, .. } = reply else {
        return Err(invalid("expected recorded tool command"));
    };
    Ok(match &receipt.decision {
        DomainDecision::Refused { fault } if fault.code == DomainErrorCode::OutcomeUnknown => {
            let operation_id = receipt
                .operation_id
                .clone()
                .ok_or_else(|| invalid("unknown tool effect lacks operation identity"))?;
            (ToolOutcome::OutcomeUnknown { operation_id }, None)
        }
        DomainDecision::Refused { fault } => failed(&format!("domain_{:?}", fault.code)),
        DomainDecision::Accepted { operation_id } => accepted(
            serde_json::to_vec(reply).map_err(invalid)?,
            operation_id.clone(),
        ),
        DomainDecision::Applied { .. } | DomainDecision::NoChange { .. } => {
            completed(serde_json::to_vec(reply).map_err(invalid)?)
        }
    })
}

fn failed(code: &str) -> (ToolOutcome, Option<Vec<u8>>) {
    (
        ToolOutcome::Failed {
            code: static_id(code),
        },
        None,
    )
}

fn selected_builtin(
    schemas: &RegisteredSchemas,
    call: &CompleteToolCall,
) -> Result<(&'static OperationDescriptor, Mode, SchemaRevision), StorageError> {
    let revision =
        SchemaRevision(u32::try_from(call.input_schema.revision.get()).map_err(invalid)?);
    for descriptor in registry::all() {
        if call.capability_id.as_str() != format!("devworks/{}", descriptor.name) {
            continue;
        }
        for binding in descriptor.modes {
            let Ok(registered) =
                schemas.builtin(descriptor.name, binding.mode, revision, "recorded")
            else {
                continue;
            };
            if registered.input_schema.schema_id == call.input_schema.schema_id
                && registered.input_schema.revision == call.input_schema.revision
                && registered.input_schema.sha256 == call.input_schema.sha256
            {
                return Ok((descriptor, binding.mode, revision));
            }
        }
    }
    Err(StorageError::ScopeDenied)
}
fn static_id(value: &str) -> Id {
    Id::new(value).expect("static tool error code")
}
fn invalid(error: impl ToString) -> StorageError {
    StorageError::InvalidRecord(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_core::protocol::{
        AttemptId, CommandId, CorrelationId, Counter, Protocol, Receipt, Revision,
    };

    fn key(native: &str) -> ToolCallKey {
        ToolCallKey {
            attempt_id: AttemptId(Id::new("attempt").unwrap()),
            native_call_id: native.into(),
            ordinal: Counter::new(0).unwrap(),
        }
    }

    #[test]
    fn accepted_command_keeps_its_operation_and_receipt_for_later_updates() {
        let operation_id = OperationId(Id::new("operation").unwrap());
        let reply = CommandReply::Recorded {
            protocol: Protocol::Draft1,
            receipt: Receipt {
                command_id: CommandId(Id::new("command").unwrap()),
                committed_sequence: Revision::new(1).unwrap(),
                accepted_at_ms: Counter::new(1).unwrap(),
                request_digest: "a".repeat(64),
                operation_id: Some(operation_id.clone()),
                decision: DomainDecision::Accepted {
                    operation_id: operation_id.clone(),
                },
            },
            replayed: false,
        };
        let (outcome, bytes) = recorded_command(&reply).unwrap();
        assert!(matches!(outcome, ToolOutcome::OperationAccepted {
            operation_id: id, ref artifacts
        } if id == operation_id && artifacts.is_empty()));
        let bytes = bytes.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["receipt"]["operationId"],
            "operation"
        );
    }

    #[test]
    fn unknown_command_effect_keeps_operation_identity() {
        let operation_id = OperationId(Id::new("operation").unwrap());
        let fault = devworks_core::services::sessions::Refusal::new(
            DomainErrorCode::OutcomeUnknown,
            "operation",
            Some(operation_id.0.clone()),
        )
        .retry(RetryAdvice::ReconcileOperation)
        .fault(CorrelationId(Id::new("correlation").unwrap()));
        let reply = CommandReply::Recorded {
            protocol: Protocol::Draft1,
            receipt: Receipt {
                command_id: CommandId(Id::new("command").unwrap()),
                committed_sequence: Revision::new(1).unwrap(),
                accepted_at_ms: Counter::new(1).unwrap(),
                request_digest: "a".repeat(64),
                operation_id: Some(operation_id.clone()),
                decision: DomainDecision::Refused { fault },
            },
            replayed: false,
        };
        let (outcome, bytes) = recorded_command(&reply).unwrap();
        assert!(matches!(outcome, ToolOutcome::OutcomeUnknown {
            operation_id: id
        } if id == operation_id));
        assert!(bytes.is_none());
    }

    #[test]
    fn domain_tool_arguments_cannot_choose_operation_or_replay_identity() {
        let session = Id::new("session").unwrap();
        let descriptor = registry::find("sessions.get").unwrap();
        let one = frame(
            &session,
            &key("call-a"),
            descriptor,
            Mode::Query,
            SchemaRevision(1),
            json!({"sessionId":"session"}),
        )
        .unwrap();
        let replay = frame(
            &session,
            &key("call-a"),
            descriptor,
            Mode::Query,
            SchemaRevision(1),
            json!({"sessionId":"session"}),
        )
        .unwrap();
        let other = frame(
            &session,
            &key("call-b"),
            descriptor,
            Mode::Query,
            SchemaRevision(1),
            json!({"sessionId":"session"}),
        )
        .unwrap();
        assert_eq!(one, replay);
        assert_ne!(one, other);
        let value: Value = serde_json::from_slice(&one).unwrap();
        assert_eq!(value["body"]["kind"], "sessions.get");
        assert_eq!(value["mode"], "Query");
        assert!(
            frame(
                &session,
                &key("call-a"),
                descriptor,
                Mode::Query,
                SchemaRevision(1),
                json!({"kind":"sessions.end","sessionId":"session"})
            )
            .is_err()
        );
    }

    #[test]
    fn specialized_tool_identity_is_replaced_and_selector_still_checked() {
        let session = Id::new("session").unwrap();
        let descriptor = registry::find("runs.inspect").unwrap();
        let valid = json!({"protocol":"other","queryId":"provider-chosen",
            "query":{"kind":"runs.inspect","rootId":"root"}});
        let built = frame(
            &session,
            &key("native"),
            descriptor,
            Mode::Query,
            SchemaRevision(1),
            valid,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&built).unwrap();
        assert_eq!(value["protocol"], CONTRACT);
        assert_ne!(value["queryId"], "provider-chosen");
        assert!(
            frame(
                &session,
                &key("native"),
                descriptor,
                Mode::Query,
                SchemaRevision(1),
                json!({"query":{"kind":"runs.eligibility","rootId":"root"}})
            )
            .is_err()
        );
    }
}
