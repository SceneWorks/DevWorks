//! Automatic context compaction. When an agent's next request would reach the configured share
//! of its routed model's context window, the host asks the same pinned route for a summary and
//! starts a new context epoch from it. The transcript and every earlier context artifact remain
//! unchanged; the epoch records the compacted context and its source.
use devworks_core::{
    context::InvocationContext,
    ports::storage::*,
    protocol::{
        Counter, Id, OperationState,
        provider::{ArtifactRef, ContextChangeReason, NativeBinding, TranscriptBody},
    },
    providers::context::{ContentBlock, Message, MessageRole, ProviderContext},
    services::artifacts::{ArtifactService, host::HostArtifactPublication, version_ref},
};
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Provenance kind of the exact context sent with a compaction request.
pub(crate) const INPUT_KIND: &str = "ProviderCompactionInput";

const REQUEST: &str = "Host notice: Your context is nearly full and will be compacted. Write a \
    summary that replaces the conversation so far in your working context; \
    the full transcript stays stored. Keep the current requirements, the \
    plan and where it stands, every direction from people, unresolved \
    decisions and questions, and the evidence and artifact, operation and \
    work IDs you still need. Reply with the summary only.";

/// Starts the text block that carries a compaction summary in a compacted context. The block
/// names its epoch, so two compacted contexts with the same summary are distinct requests.
const SUMMARY_PREFIX: &str = "Host notice: Earlier conversation was compacted";

/// The context a compaction request sends: `source` with the summary request appended, no tool
/// choice, published as an immutable host artifact that names its source and epoch root.
pub(crate) fn publish_input(
    store: &dyn ApplicationStore,
    artifacts: &Arc<ArtifactService>,
    ctx: &InvocationContext,
    session_id: &Id,
    binding: &NativeBinding,
    source: &crate::context_epoch::EpochSource,
) -> Result<ArtifactRef, StorageError> {
    let observed = source.observed_sequence.ok_or(StorageError::ScopeDenied)?;
    let mut input = source.context.clone();
    input.parameters.tool_choice = None;
    input.parameters.parallel_tool_calls = None;
    let request = ContentBlock::Text {
        text: REQUEST.into(),
        cache: None,
    };
    match input.messages.last_mut() {
        Some(last) if matches!(last.role, MessageRole::User) => last.content.push(request),
        _ => input.messages.push(Message {
            role: MessageRole::User,
            content: vec![request],
        }),
    }
    input
        .validate()
        .map_err(|error| StorageError::InvalidRecord(format!("compaction input: {error:?}")))?;
    let bytes = serde_json::to_vec(&input)
        .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
    let staged = artifacts
        .stage_host_bytes(
            store,
            &bytes,
            json!({"kind":INPUT_KIND,"sessionId":session_id,"binding":binding,
                "observedSequence":observed}),
        )?
        .map_err(StorageError::InvalidRecord)?;
    let staged_for_write = staged.clone();
    let service = artifacts.clone();
    let ctx = ctx.clone();
    let session_id = session_id.clone();
    let binding = binding.clone();
    let source_artifact = source.artifact.clone();
    let committed = run_write(store, WriteOptions::observed(), move |tx| {
        let session = tx
            .sessions()
            .session(&session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if session.closed()
            || ctx.session_id() != Some(&session_id)
            || session.context_epoch != binding.context_epoch.get()
            || (session.last_durable_sequence() != observed
                && !(ctx.workspace_reply().is_some()
                    && crate::tool_turn::workspace_input_tail(
                        tx.sessions(),
                        &session,
                        observed,
                        true,
                    )?))
        {
            return Err(StorageError::ScopeDenied);
        }
        let observed = session.last_durable_sequence();
        let root = crate::context_epoch::epoch_root(
            tx.sessions(),
            &session_id,
            binding.context_epoch.get(),
        )?;
        let scope = session
            .project_id
            .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
        let version = service.commit_host_bytes_in_scope(
            tx,
            &staged_for_write,
            HostArtifactPublication {
                title: "Provider compaction input",
                media_type: "application/vnd.devworks.provider-context+json",
                provenance: json!({"kind":INPUT_KIND,"sessionId":session_id,
                    "binding":binding,"observedSequence":observed,"epochRoot":root,
                    "sourceContext":source_artifact}),
                scope,
                owner: ctx.principal().clone(),
            },
        )?;
        Ok(version_ref(&version))
    });
    artifacts.settle_host_bytes(store, &staged, committed.is_ok());
    committed
}

/// The compacted context: the source's instructions, stamps and parameters, its first message
/// (the assignment brief) without an earlier summary, and the new summary. Published as the
/// root of the next epoch, so later turns build append-only from it.
pub(crate) fn publish_compacted(
    store: &dyn ApplicationStore,
    artifacts: &Arc<ArtifactService>,
    ctx: &InvocationContext,
    session_id: &Id,
    binding: &NativeBinding,
    source: crate::context_epoch::EpochSource,
    summary: &str,
) -> Result<ArtifactRef, StorageError> {
    let previous_epoch = binding.context_epoch.get();
    let (next, omitted) = omitted_native(&source.context, binding)?;
    let mut outstanding = outstanding_calls(store, ctx, session_id, &source.context)?;
    let (observed, updates) = if let Some(reply) = ctx.workspace_reply() {
        workspace_updates(
            store,
            session_id,
            source.observed_sequence,
            reply.user_sequence,
        )?
    } else {
        (source.observed_sequence, Vec::new())
    };
    if !updates.is_empty() {
        let linked = crate::tool_turn::linked_operations(store, ctx, session_id)?;
        for update in &updates {
            if let TranscriptBody::OperationUpdate { operation_id, .. } = update {
                let call = linked
                    .iter()
                    .find(|call| call.operation_id.as_ref() == Some(operation_id))
                    .ok_or(StorageError::ScopeDenied)?;
                outstanding.insert(call.native_call_id.clone());
            }
        }
    }
    let mut compacted = compacted_context(
        &source.context,
        next.context_epoch.get(),
        summary,
        &outstanding,
    );
    // A late operation update is outside the paid summary's exact source. Keep its original
    // call and response, then attach the original update to that response before advancing.
    // A late watcher reminder is outside it too, and the advanced cursor hides it from later
    // turns; it has no event row for the restore below, so carry its exact notice here.
    for body in updates {
        crate::tool_turn::append_context_fact(
            &mut compacted,
            store,
            artifacts,
            ctx,
            session_id,
            body,
        )?;
    }
    if ctx.assignment_id().is_some() || ctx.workspace_reply().is_some() {
        for notice in crate::watch::unacknowledged_notices(
            store,
            session_id,
            observed.ok_or(StorageError::ScopeDenied)?,
        )? {
            crate::tool_turn::append_context_fact(
                &mut compacted,
                store,
                artifacts,
                ctx,
                session_id,
                notice,
            )?;
        }
    }
    if let Some(reply) = ctx.workspace_reply()
        && reply.user_sequence > 1
    {
        // A summary does not replace the exact message that is still awaiting its reply.
        // The cutoff also prevents later queued workspace messages from entering this epoch.
        let body = read_snapshot(store, &ReadOptions::default(), |read| {
            let entries = read
                .sessions()
                .entries(session_id, reply.user_sequence - 1, 1)?;
            let entry = entries
                .first()
                .filter(|entry| entry.sequence == reply.user_sequence)
                .ok_or(StorageError::ScopeDenied)?;
            let body = serde_json::from_value::<TranscriptBody>(entry.body.clone())
                .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
            match &body {
                TranscriptBody::UserMessage { .. } => Ok(Some(body)),
                // The exact event was carried above. A reminder has no acknowledgement row.
                TranscriptBody::HostNotice { .. } if crate::watch::is_notice(&body) => {
                    Ok(is_reminder(read, &body)?.then_some(body))
                }
                _ => Err(StorageError::ScopeDenied),
            }
        })?;
        if let Some(body) = body {
            crate::tool_turn::append_context_fact(
                &mut compacted,
                store,
                artifacts,
                ctx,
                session_id,
                body,
            )?;
        }
    }
    let (context, _) = compacted
        .converted_for_new_epoch(binding, &next)
        .map_err(|error| StorageError::InvalidRecord(format!("compaction: {error:?}")))?;
    crate::context_epoch::publish(
        store,
        artifacts,
        ctx,
        session_id,
        crate::context_epoch::EpochChange {
            previous_epoch,
            binding: next,
            observed_sequence: observed,
            source: source.artifact,
            reason: ContextChangeReason::Compaction,
            context,
            omitted,
            adoption: None,
        },
    )
}

/// A watcher reminder: a watcher notice without an event row. A delivered event is restored from
/// its row; a reminder exists only in the transcript.
fn is_reminder(read: &dyn ReadSnapshot, body: &TranscriptBody) -> Result<bool, StorageError> {
    match body {
        TranscriptBody::HostNotice { source, .. } if crate::watch::is_notice(body) => {
            Ok(read.watchers().watch_event(source)?.is_none())
        }
        _ => Ok(false),
    }
}

/// The advanced cursor and, in transcript order, the operation updates and watcher reminders
/// after `observed`. The reminder at `reply_sequence` is left to the caller, which carries the
/// input the reply answers.
fn workspace_updates(
    store: &dyn ApplicationStore,
    session_id: &Id,
    observed: Option<i64>,
    reply_sequence: i64,
) -> Result<(Option<i64>, Vec<TranscriptBody>), StorageError> {
    let observed = observed.ok_or(StorageError::ScopeDenied)?;
    read_snapshot(store, &ReadOptions::default(), |read| {
        let session = read
            .sessions()
            .session(session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let mut position = observed;
        let mut updates = Vec::new();
        while position < session.last_durable_sequence() {
            if position - observed >= 10_000 {
                return Err(StorageError::InvalidRecord(
                    "compaction update scan bound".into(),
                ));
            }
            let page = read.sessions().entries(session_id, position, 500)?;
            if page.is_empty() {
                return Err(StorageError::ScopeDenied);
            }
            for entry in page {
                if entry.sequence != position + 1 || entry.context_epoch != session.context_epoch {
                    return Err(StorageError::ScopeDenied);
                }
                position = entry.sequence;
                match entry.kind.as_str() {
                    "UserMessage" => {}
                    "HostNotice" => {
                        if entry.sequence == reply_sequence {
                            continue;
                        }
                        let body: TranscriptBody = serde_json::from_value(entry.body)
                            .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
                        if is_reminder(read, &body)? {
                            if updates.len() == 128 {
                                return Err(StorageError::InvalidRecord(
                                    "compaction reminder bound".into(),
                                ));
                            }
                            updates.push(body);
                        }
                    }
                    "OperationUpdate" => {
                        let body: TranscriptBody = serde_json::from_value(entry.body)
                            .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
                        if !matches!(body, TranscriptBody::OperationUpdate { .. })
                            || updates.len() == 128
                        {
                            return Err(StorageError::InvalidRecord(
                                "invalid compaction operation update".into(),
                            ));
                        }
                        updates.push(body);
                    }
                    _ => return Err(StorageError::ScopeDenied),
                }
            }
        }
        Ok((Some(position), updates))
    })
}

/// The next epoch's binding and the opaque provider blocks of `context` that cannot cross into
/// it, which the epoch records as omitted sources. Refused before any summary is requested when
/// the epoch could not record them all.
pub(crate) fn omitted_native(
    context: &ProviderContext,
    binding: &NativeBinding,
) -> Result<(NativeBinding, Vec<ArtifactRef>), StorageError> {
    let next = NativeBinding {
        context_epoch: Counter::new(
            binding
                .context_epoch
                .get()
                .checked_add(1)
                .ok_or_else(|| StorageError::InvalidRecord("context epoch overflow".into()))?,
        )
        .map_err(|error| StorageError::InvalidRecord(error.into()))?,
        ..binding.clone()
    };
    let (_, omitted) = context
        .clone()
        .converted_for_new_epoch(binding, &next)
        .map_err(|error| StorageError::InvalidRecord(format!("compaction: {error:?}")))?;
    if omitted.len() > crate::context_epoch::MAX_OMITTED_NATIVE {
        return Err(StorageError::InvalidRecord(
            "context has too many native blocks to compact".into(),
        ));
    }
    Ok((next, omitted))
}

/// Calls of `context` whose tool result can still receive an update: the linked operation can
/// still change, or it reached a terminal state whose update the transcript does not hold yet
/// (an operation can finish while the summary request runs). A compacted context keeps the
/// call and its result, so that update is appended to them. Calls an earlier compaction already
/// summarized are not in `context` and are not checked.
fn outstanding_calls(
    store: &dyn ApplicationStore,
    ctx: &InvocationContext,
    session_id: &Id,
    context: &ProviderContext,
) -> Result<BTreeSet<String>, StorageError> {
    let present: BTreeSet<&str> = context
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolCall { call_id, .. } | ContentBlock::ToolResult { call_id, .. } => {
                Some(call_id.as_str())
            }
            _ => None,
        })
        .collect();
    let linked = crate::tool_turn::linked_operations(store, ctx, session_id)?;
    read_snapshot(store, &ReadOptions::default(), |read| {
        let mut outstanding = BTreeSet::new();
        let mut terminal = Vec::new();
        for call in &linked {
            let Some(operation_id) = &call.operation_id else {
                continue;
            };
            if !present.contains(call.native_call_id.as_str()) {
                continue;
            }
            let operation = read
                .operations()
                .operation(operation_id)?
                .ok_or(StorageError::ScopeDenied)?;
            if matches!(
                operation.state,
                OperationState::ObservedComplete
                    | OperationState::Failed
                    | OperationState::CancelledBeforeDispatch
            ) {
                terminal.push((
                    call,
                    (operation_id.0.as_str().to_owned(), operation.revision),
                ));
            } else {
                outstanding.insert(call.native_call_id.clone());
            }
        }
        let published = published_updates(read, session_id, &terminal)?;
        for (call, update) in terminal {
            if !published.contains(&update) {
                outstanding.insert(call.native_call_id.clone());
            }
        }
        Ok(outstanding)
    })
}

/// Which of the updates `calls` wait for (operation and revision) the transcript holds after
/// their first response. The scan stops once it has found them all, or after the bound that
/// recording an update uses: an update it did not reach counts as unpublished, which keeps its
/// call, rather than failing the compaction.
fn published_updates(
    read: &dyn ReadSnapshot,
    session_id: &Id,
    calls: &[(&StoredToolCall, (String, i64))],
) -> Result<BTreeSet<(String, i64)>, StorageError> {
    let wanted: BTreeSet<&(String, i64)> = calls.iter().map(|(_, update)| update).collect();
    let mut published = BTreeSet::new();
    let Some(first) = calls
        .iter()
        .filter_map(|(call, _)| call.response_sequence)
        .min()
    else {
        return Ok(published);
    };
    let last = read
        .sessions()
        .session(session_id)?
        .ok_or(StorageError::ScopeDenied)?
        .last_durable_sequence();
    let mut position = first;
    while position < last && position - first < 10_000 && published.len() < wanted.len() {
        let page = read.sessions().entries(session_id, position, 500)?;
        if page.is_empty() {
            return Err(StorageError::InvalidRecord(
                "operation update transcript gap".into(),
            ));
        }
        for entry in page {
            if entry.sequence != position + 1 {
                return Err(StorageError::InvalidRecord(
                    "operation update transcript gap".into(),
                ));
            }
            position = entry.sequence;
            if entry.kind != "OperationUpdate" {
                continue;
            }
            if let TranscriptBody::OperationUpdate {
                operation_id,
                revision,
                ..
            } = serde_json::from_value(entry.body)
                .map_err(|error| StorageError::InvalidRecord(error.to_string()))?
            {
                let update = (operation_id.0.as_str().to_owned(), revision.get());
                if wanted.contains(&update) {
                    published.insert(update);
                }
            }
        }
    }
    Ok(published)
}

/// The block carrying an earlier compaction summary, which the next compaction replaces.
pub(crate) fn is_summary(block: &ContentBlock) -> bool {
    matches!(block, ContentBlock::Text { text, .. } if text.starts_with(SUMMARY_PREFIX))
}

fn compacted_context(
    source: &ProviderContext,
    epoch: i64,
    summary: &str,
    outstanding: &BTreeSet<String>,
) -> ProviderContext {
    let mut content: Vec<ContentBlock> = source
        .messages
        .first()
        .filter(|message| matches!(message.role, MessageRole::User))
        .map(|message| {
            message
                .content
                .iter()
                .filter(|block| match block {
                    ContentBlock::Text { .. } => !is_summary(block),
                    ContentBlock::Image { .. } => true,
                    _ => false,
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    content.push(ContentBlock::Text {
        text: format!(
            "{SUMMARY_PREFIX} into context epoch {epoch}. Summary:\n{}",
            summary.trim()
        ),
        cache: None,
    });
    let mut messages = vec![Message {
        role: MessageRole::User,
        content,
    }];
    // Exchanges whose operation can still report keep their call and result, in order.
    for message in source.messages.iter().skip(1) {
        let kept: Vec<ContentBlock> = message
            .content
            .iter()
            .filter(|block| match block {
                ContentBlock::ToolCall { call_id, .. }
                | ContentBlock::ToolResult { call_id, .. } => outstanding.contains(call_id),
                _ => false,
            })
            .cloned()
            .collect();
        if !kept.is_empty() {
            messages.push(Message {
                role: message.role.clone(),
                content: kept,
            });
        }
    }
    ProviderContext {
        messages,
        ..source.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_core::providers::context::GenerationParameters;

    fn text(role: MessageRole, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text {
                text: text.into(),
                cache: None,
            }],
        }
    }

    #[test]
    fn a_compacted_context_keeps_the_brief_and_replaces_an_earlier_summary() {
        let mut brief = text(MessageRole::User, "Assignment brief");
        brief.content.push(ContentBlock::Text {
            text: format!("{SUMMARY_PREFIX} into context epoch 1. Summary:\nold summary"),
            cache: None,
        });
        let source = ProviderContext {
            version: 1,
            instructions: vec![],
            instruction_stamps: vec![],
            skill_stamp: None,
            messages: vec![
                brief,
                text(MessageRole::Assistant, "long answer"),
                text(MessageRole::User, "redirect"),
            ],
            parameters: GenerationParameters::default(),
        };
        let compacted = compacted_context(&source, 2, " new summary ", &BTreeSet::new());
        compacted.validate().unwrap();
        let texts: Vec<_> = compacted.messages[0]
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text, .. } => text.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(compacted.messages.len(), 1);
        assert_eq!(
            texts,
            [
                "Assignment brief",
                &format!("{SUMMARY_PREFIX} into context epoch 2. Summary:\nnew summary")
            ]
        );
    }

    #[test]
    fn a_compacted_context_keeps_exchanges_whose_operation_can_still_report() {
        let arguments: ArtifactRef = serde_json::from_value(json!({
            "artifactId": "arguments", "versionId": "arguments-1", "sha256": "a".repeat(64),
            "byteLength": "2", "mediaType": "application/json"}))
        .unwrap();
        let call = |id: &str| ContentBlock::ToolCall {
            call_id: id.into(),
            wire_name: "tool".into(),
            arguments: arguments.clone(),
        };
        let result = |id: &str| ContentBlock::ToolResult {
            call_id: id.into(),
            operation_id: None,
            content: vec![],
            is_error: false,
            cache: None,
        };
        let mut asked = text(MessageRole::Assistant, "calling");
        asked.content.extend([call("held"), call("done")]);
        let source = ProviderContext {
            version: 1,
            instructions: vec![],
            instruction_stamps: vec![],
            skill_stamp: None,
            messages: vec![
                text(MessageRole::User, "Assignment brief"),
                asked,
                Message {
                    role: MessageRole::User,
                    content: vec![result("held"), result("done")],
                },
                text(MessageRole::Assistant, "long answer"),
            ],
            parameters: GenerationParameters::default(),
        };
        let compacted =
            compacted_context(&source, 1, "summary", &BTreeSet::from(["held".to_owned()]));
        compacted.validate().unwrap();
        let ids: Vec<_> = compacted.messages[1..]
            .iter()
            .flat_map(|message| &message.content)
            .map(|block| match block {
                ContentBlock::ToolCall { call_id, .. } => format!("call {call_id}"),
                ContentBlock::ToolResult { call_id, .. } => format!("result {call_id}"),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(ids, ["call held", "result held"]);
    }
}
