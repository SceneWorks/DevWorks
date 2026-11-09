//! Host-owned framing of provider tool arguments. A provider chooses arguments only; the
//! selected operation, mode, request identity and contract envelope come from the host.
use crate::{providers::schemas::RegisteredSchemas, runtime::Runtime};
use devworks_core::{
    admission::{self, Selection},
    context::{InvocationContext, PrincipalKind},
    fingerprint::sha256_hex,
    ports::{IdKind, storage::*},
    protocol::{
        AttemptId, CONTRACT, CommandReply, Id, RetryAdvice,
        provider::{CompleteToolCall, NativeBinding, ToolCallKey, ToolOutcome, TranscriptBody},
        results::{DomainDecision, DomainQueryReply},
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
) -> Result<ProviderContext, StorageError> {
    let TurnSource {
        session_id,
        attempt_id,
        after_sequence,
    } = source;
    let resolved = resolved_turn(store, ctx, session_id, attempt_id, after_sequence)?;
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
    let entries = read_snapshot(store, &ReadOptions::default(), |read| {
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
                let body: TranscriptBody = serde_json::from_value(entry.body).map_err(invalid)?;
                let belongs = match &body {
                    TranscriptBody::AssistantAttempt { attempt_id: id, .. } => id == attempt_id,
                    TranscriptBody::ToolCallRecorded { tool_call } => {
                        &tool_call.call.attempt_id == attempt_id
                    }
                    TranscriptBody::NativeItem { artifact, .. } => {
                        let row = read
                            .artifacts()
                            .artifact(&artifact.artifact_id)?
                            .ok_or(StorageError::ScopeDenied)?;
                        row.provenance["derived"]["attemptId"] == attempt_id.0.as_str()
                    }
                    _ => false,
                };
                if belongs {
                    selected.push(body);
                }
            }
        }
        Ok(selected)
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
    if assistant.is_empty() {
        return Err(invalid("empty assistant tool turn"));
    }
    let mut results = Vec::new();
    for item in resolved {
        let (content, is_error) = match item.outcome {
            ToolOutcome::Completed { artifacts: refs } => {
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
    context.messages.push(Message {
        role: MessageRole::Assistant,
        content: assistant,
    });
    context.messages.push(Message {
        role: MessageRole::User,
        content: results,
    });
    context.validate().map_err(|e| invalid(format!("{e:?}")))?;
    Ok(context)
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
pub(crate) fn dispatch_one(
    runtime: &Runtime,
    store: &dyn ApplicationStore,
    artifacts: &Arc<ArtifactService>,
    schemas: &RegisteredSchemas,
    ctx: &InvocationContext,
    session_id: &Id,
    key: &ToolCallKey,
) -> Result<BatchOutcome, StorageError> {
    let call = read_snapshot(store, &ReadOptions::default(), |read| {
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
        Ok(tool_call)
    })?;

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
            Ok(reply @ CommandReply::Recorded { .. }) => {
                if let CommandReply::Recorded { receipt, .. } = &reply
                    && let DomainDecision::Refused { fault } = &receipt.decision
                {
                    failed(&format!("domain_{:?}", fault.code))
                } else {
                    completed(serde_json::to_vec(&reply).map_err(invalid)?)
                }
            }
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
            outcome = ToolOutcome::Completed {
                artifacts: vec![version_ref(&version)],
            };
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

fn completed(bytes: Vec<u8>) -> (ToolOutcome, Option<Vec<u8>>) {
    (ToolOutcome::Completed { artifacts: vec![] }, Some(bytes))
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
    use devworks_core::protocol::{AttemptId, Counter};

    fn key(native: &str) -> ToolCallKey {
        ToolCallKey {
            attempt_id: AttemptId(Id::new("attempt").unwrap()),
            native_call_id: native.into(),
            ordinal: Counter::new(0).unwrap(),
        }
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
