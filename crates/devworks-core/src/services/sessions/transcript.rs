//! The session writer: `append_session_batch` (devworks-architecture.md "Transaction ownership
//! and runtime handoff", provider-session-package-contracts.md "Transcript transactions and one
//! response per native call").
//!
//! [`append_session_batch`] runs inside the caller's single unit of work: the gateway's command
//! transaction for `sessions.append_message`, or the host runtime's own write through
//! [`SessionWriter::append`]. Given the session and its expected next sequence, the attributed
//! transcript/native entries, durable tool calls and their single initial responses, the
//! context-epoch boundary and its recorded context sources, an optional checkpoint and the
//! attempt's host-classified usage all commit together with one history event and the batch
//! receipt, or nothing of the batch remains (a failed tail never leaves an accepted prefix).
//!
//! The rules are the recorded `TranscriptModel` candidate's, applied to durable state:
//!
//! * the batch receipt is checked before the sequence CAS: an equal replay returns the original
//!   receipt unchanged (even after the session closed), a changed payload under that batch ID is
//!   refused. Batch identities are scoped per session, so another session's batch is never
//!   revealed or matched;
//! * the expected next sequence must equal the session's; a lower one is stale, a higher one a
//!   gap; entries are contiguous with unique identities;
//! * a different context epoch needs an explicit boundary — the batch's first entry is
//!   `ContextEpochChanged` from the current epoch to the next — and a boundary inside the current
//!   epoch is refused, as is a boundary while emitted calls still lack their initial response.
//!   Compaction therefore adds an epoch that references its immutable snapshot (with an epoch
//!   identity; the provider cache key is the router's); the original transcript stays exactly as
//!   it was. Late native observations bound to an older epoch are kept as evidence;
//! * a tool call is keyed `(attempt, native call)` with its recorded ordinal and is recorded once;
//!   an initial response must match a durable call and appears once. Later outcomes are
//!   `OperationUpdate` entries: separate facts, never a rewrite. Every operation an entry names
//!   must belong to the session's project;
//! * provider-native blocks, attachments, arguments, results, snapshots and checkpoints are
//!   referenced by artifact identity and must resolve to a live version with the same digest,
//!   length and media type that the session may see. Nothing is copied or rewritten.
//!
//! A UI preview (streamed text not yet in a batch) is never a checkpoint: the last durable
//! position is the session's `next_sequence - 1`, advanced only by a committed batch.
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::jcs;
use crate::ports::IdKind;
use crate::ports::storage::{
    ApplicationStore, NewEvent, StorageError, StoredBatch, StoredCheckpoint, StoredContextSource,
    StoredEntry, StoredEpoch, StoredInvocation, StoredLink, StoredSession, StoredToolCall,
    StoredUsage, WriteOptions, WriteTransaction, run_write,
};
use crate::protocol::provider::{
    ArtifactRef, ToolOutcome, TranscriptBatch, TranscriptBody, UserContent,
};
use crate::protocol::records::UsageItem;
use crate::protocol::support::Check;
use crate::protocol::{ActorId, EventId, Id, OperationId};
use serde::Serialize;
use serde_json::{Value, json};

/// History aggregate of a session (`aggregate_id` = session ID).
pub const SESSION_AGGREGATE: &str = "session";
/// History kind of an appended batch.
pub const BATCH_APPENDED: &str = "TranscriptBatchAppended";
/// Most usage observations one batch may carry.
pub const MAX_BATCH_USAGE: usize = 16;
/// Most context sources one batch may record.
pub const MAX_BATCH_SOURCES: usize = 128;

/// How a recorded context source entered the context (CONTEXT-02).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum SourceDisposition {
    Included,
    /// Included only in part (for example a window of a long file).
    Truncated,
    /// Considered and deliberately left out (budget, scope or authority).
    Omitted,
}
impl SourceDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Included => "Included",
            Self::Truncated => "Truncated",
            Self::Omitted => "Omitted",
        }
    }
}

/// One context source the host assembled for the batch's epoch: instruction revisions, work
/// briefs, retrieved code/knowledge and prior evidence, with omissions and truncation.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextSourceInput {
    pub source_id: Id,
    pub kind: String,
    pub source_revision: i64,
    pub artifact: Option<ArtifactRef>,
    pub token_estimate: i64,
    pub disposition: SourceDisposition,
}

/// The host-owned batch envelope a session writer appends. It is never decoded from caller
/// input: a public message can only reach it through `sessions.append_message`, which builds a
/// single user message.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBatch {
    pub batch: TranscriptBatch,
    /// A durable checkpoint at the batch's last entry (handover state, agent checkpoint).
    pub checkpoint: Option<ArtifactRef>,
    pub context_sources: Vec<ContextSourceInput>,
    /// Host-classified usage observations of the batch's attempts.
    pub usage: Vec<UsageItem>,
}
impl SessionBatch {
    /// A batch of entries only.
    pub fn entries(batch: TranscriptBatch) -> Self {
        Self {
            batch,
            checkpoint: None,
            context_sources: Vec::new(),
            usage: Vec::new(),
        }
    }
}

/// The durable receipt of an appended batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchReceipt {
    pub batch_id: Id,
    pub session_id: Id,
    pub first_sequence: i64,
    pub last_sequence: i64,
    pub context_epoch: i64,
    /// Committed (global history) sequence of the append.
    pub committed_sequence: i64,
    pub accepted_at_ms: i64,
}
impl BatchReceipt {
    fn of(b: &StoredBatch) -> Self {
        Self {
            batch_id: b.batch_id.clone(),
            session_id: b.session_id.clone(),
            first_sequence: b.first_sequence,
            last_sequence: b.last_sequence,
            context_epoch: b.context_epoch,
            committed_sequence: b.committed_sequence,
            accepted_at_ms: b.accepted_at_ms,
        }
    }
    /// The next sequence a following batch must present.
    pub fn next_sequence(&self) -> i64 {
        self.last_sequence + 1
    }
}

/// Why a batch was refused. Nothing of it was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchRefusal {
    /// Structure: size, contiguity, identity or field bounds.
    Invalid(&'static str),
    /// This batch ID was appended with a different payload.
    PayloadChanged,
    SessionNotFound,
    SessionClosed,
    /// The presented sequence is behind the session's (a stale writer).
    StaleSequence {
        expected: i64,
        presented: i64,
    },
    /// The presented sequence is ahead of the session's (a lost batch in between).
    SequenceGap {
        expected: i64,
        presented: i64,
    },
    /// The batch's epoch differs without an explicit `ContextEpochChanged` boundary first.
    EpochBoundaryRequired {
        current: i64,
        presented: i64,
    },
    /// A boundary inside the current epoch, or more than one boundary.
    DuplicateBoundary,
    /// A context boundary while emitted tool calls still lack their initial response: compaction
    /// or a provider change must not cut an unresolved tool exchange.
    UnresolvedToolCalls,
    DuplicateEntry(Id),
    /// The `(attempt, native call)` key or its ordinal is already recorded.
    DuplicateCall,
    /// A response without a matching durable call (or with another ordinal).
    MissingCall,
    /// A second initial response for a call.
    SecondResponse,
    /// An artifact reference that does not resolve to a live version the session may see.
    UnknownReference(&'static str),
    UnknownOperation(OperationId),
    /// The append path does not allow this entry (a public message is a user message only).
    NotAllowed(&'static str),
    /// The writer may not append to this session.
    Unauthorized(&'static str),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchOutcome {
    Accepted(BatchReceipt),
    /// The original receipt of an identical earlier append.
    Replayed(BatchReceipt),
    Refused(BatchRefusal),
}

/// Which entries the append path may write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendPath {
    /// `sessions.append_message`: one user message; it advances the session revision.
    UserMessage,
    /// The host runtime (assistant attempts, tool calls/results, notices, native items, epochs,
    /// operation updates, checkpoints, sources and usage).
    Runtime,
}

/// Digest of the batch's meaning, including the attributed writer.
pub fn batch_digest(actor: &ActorId, batch: &SessionBatch) -> Result<String, StorageError> {
    let value = json!({"actor": actor.0.as_str(), "batch": batch});
    let canonical = jcs::canonicalize(&value)
        .map_err(|e| StorageError::InvalidRecord(format!("batch canonicalization: {e:?}")))?;
    Ok(sha256_hex(canonical.as_bytes()))
}

fn body_kind(body: &TranscriptBody) -> &'static str {
    match body {
        TranscriptBody::UserMessage { .. } => "UserMessage",
        TranscriptBody::AssistantAttempt { .. } => "AssistantAttempt",
        TranscriptBody::ToolCallRecorded { .. } => "ToolCallRecorded",
        TranscriptBody::ToolResponseRecorded { .. } => "ToolResponseRecorded",
        TranscriptBody::HostNotice { .. } => "HostNotice",
        TranscriptBody::NativeItem { .. } => "NativeItem",
        TranscriptBody::ContextEpochChanged { .. } => "ContextEpochChanged",
        TranscriptBody::OperationUpdate { .. } => "OperationUpdate",
    }
}
fn outcome_kind(o: &ToolOutcome) -> (&'static str, Option<&OperationId>) {
    match o {
        ToolOutcome::Completed { .. } => ("Completed", None),
        ToolOutcome::OperationAccepted { operation_id, .. } => {
            ("OperationAccepted", Some(operation_id))
        }
        ToolOutcome::Failed { .. } => ("Failed", None),
        ToolOutcome::ApprovalRequired { operation_id, .. } => {
            ("ApprovalRequired", Some(operation_id))
        }
        ToolOutcome::DependencyBlocked { .. } => ("DependencyBlocked", None),
        ToolOutcome::CancelledBeforeDispatch => ("CancelledBeforeDispatch", None),
        ToolOutcome::OutcomeUnknown { operation_id } => ("OutcomeUnknown", Some(operation_id)),
    }
}

/// Whether `reference` names a live artifact version with exactly that identity which work of
/// `scope_project` may see (owned or shared); without a project, what the invocation sees at
/// workspace level (every project's artifacts and the workspace artifacts its principal belongs
/// to the audience of). A receipt reply resolves only for its receipt's audience.
pub fn reference_resolves(
    tx: &mut dyn WriteTransaction,
    ctx: &crate::context::InvocationContext,
    scope_project: Option<&crate::protocol::ProjectId>,
    reference: &ArtifactRef,
) -> Result<bool, StorageError> {
    if reference.validate().is_err() {
        return Ok(false);
    }
    let viewer = crate::services::artifacts::Viewer::resolve(tx.authority(), ctx)?;
    let read = tx.artifacts();
    let Some(artifact) = read.artifact(&reference.artifact_id)? else {
        return Ok(false);
    };
    if artifact.state != crate::ports::storage::ArtifactState::Live {
        return Ok(false);
    }
    let Some(version) = read.version(&reference.version_id)? else {
        return Ok(false);
    };
    if version.artifact_id != artifact.id
        || !version.live()
        || version.blob_digest != String::from(reference.sha256.clone())
        || version.byte_count != reference.byte_length.get()
        || version.media_type != reference.media_type
    {
        return Ok(false);
    }
    let scope = scope_project.or(ctx.project_id());
    crate::services::artifacts::visible_in(read, &viewer, &artifact, scope)
}

/// Every artifact reference an entry names.
fn references(body: &TranscriptBody) -> Vec<(&'static str, &ArtifactRef)> {
    match body {
        TranscriptBody::UserMessage { content } => content
            .iter()
            .filter_map(|c| match c {
                UserContent::Attachment { artifact } => Some(("attachment", artifact)),
                UserContent::Text { .. } => None,
            })
            .collect(),
        TranscriptBody::AssistantAttempt { .. } => Vec::new(),
        TranscriptBody::ToolCallRecorded { tool_call } => vec![
            ("tool_arguments", &tool_call.arguments),
            ("native_item", &tool_call.native_item),
        ],
        TranscriptBody::ToolResponseRecorded { outcome, .. } => match outcome {
            ToolOutcome::Completed { artifacts }
            | ToolOutcome::OperationAccepted { artifacts, .. } => {
                artifacts.iter().map(|a| ("tool_result", a)).collect()
            }
            _ => Vec::new(),
        },
        TranscriptBody::HostNotice { artifact, .. } => vec![("host_notice", artifact)],
        TranscriptBody::NativeItem { artifact, .. } => vec![("native_item", artifact)],
        TranscriptBody::ContextEpochChanged { context, .. } => vec![("context_snapshot", context)],
        TranscriptBody::OperationUpdate { artifact, .. } => vec![("operation_update", artifact)],
    }
}

fn structure(batch: &SessionBatch, path: AppendPath) -> Result<(), BatchRefusal> {
    batch.batch.validate().map_err(BatchRefusal::Invalid)?;
    if batch.usage.len() > MAX_BATCH_USAGE {
        return Err(BatchRefusal::Invalid("too many usage observations"));
    }
    if batch.context_sources.len() > MAX_BATCH_SOURCES {
        return Err(BatchRefusal::Invalid("too many context sources"));
    }
    for u in &batch.usage {
        u.check()
            .map_err(|_| BatchRefusal::Invalid("invalid usage observation"))?;
    }
    for s in &batch.context_sources {
        if s.kind.is_empty()
            || s.kind.len() > 200
            || s.kind.contains(['\0', ':'])
            || s.source_revision <= 0
            || s.token_estimate < 0
        {
            return Err(BatchRefusal::Invalid("invalid context source"));
        }
    }
    for e in &batch.batch.entries {
        if let TranscriptBody::AssistantAttempt { visible_text, .. } = &e.body
            && visible_text.len() > 65536
        {
            return Err(BatchRefusal::Invalid("oversized assistant chunk"));
        }
    }
    if path == AppendPath::UserMessage
        && (batch.checkpoint.is_some()
            || !batch.context_sources.is_empty()
            || !batch.usage.is_empty()
            || batch
                .batch
                .entries
                .iter()
                .any(|e| !matches!(e.body, TranscriptBody::UserMessage { .. })))
    {
        return Err(BatchRefusal::NotAllowed(
            "a public message appends one user message only",
        ));
    }
    Ok(())
}

/// Appends `batch`, attributed to `actor`, inside `tx` (see the module documentation). A
/// refusal undoes everything the call wrote; a storage error is returned for the caller's unit
/// of work to roll back.
pub fn append_session_batch(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    actor: &ActorId,
    batch: &SessionBatch,
    path: AppendPath,
) -> Result<BatchOutcome, StorageError> {
    if let Err(r) = structure(batch, path) {
        return Ok(BatchOutcome::Refused(r));
    }
    let digest = batch_digest(actor, batch)?;
    let b = &batch.batch;
    if let Some(prior) = tx.sessions().batch(&b.session_id, &b.batch_id)? {
        return Ok(if prior.payload_digest == digest {
            BatchOutcome::Replayed(BatchReceipt::of(&prior))
        } else {
            BatchOutcome::Refused(BatchRefusal::PayloadChanged)
        });
    }
    let savepoint = tx.savepoint()?;
    match write_batch(tx, ctx, actor, batch, path, &digest) {
        Ok(Ok(receipt)) => {
            tx.release(savepoint)?;
            Ok(BatchOutcome::Accepted(receipt))
        }
        Ok(Err(refusal)) => {
            tx.rollback_to(savepoint)?;
            Ok(BatchOutcome::Refused(refusal))
        }
        Err(e) => {
            tx.rollback_to(savepoint)?;
            Err(e)
        }
    }
}

type Refusable<T> = Result<Result<T, BatchRefusal>, StorageError>;
macro_rules! refuse {
    ($r:expr) => {
        return Ok(Err($r))
    };
}

fn write_batch(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    actor: &ActorId,
    batch: &SessionBatch,
    path: AppendPath,
    digest: &str,
) -> Refusable<BatchReceipt> {
    let b = &batch.batch;
    let Some(mut session) = tx.sessions().session(&b.session_id)? else {
        refuse!(BatchRefusal::SessionNotFound)
    };
    if session.closed() {
        refuse!(BatchRefusal::SessionClosed)
    }
    let (expected, presented) = (session.next_sequence, b.expected_next_sequence.get());
    if presented < expected {
        refuse!(BatchRefusal::StaleSequence {
            expected,
            presented
        })
    }
    if presented > expected {
        refuse!(BatchRefusal::SequenceGap {
            expected,
            presented
        })
    }
    let epoch = b.context_epoch.get();
    let boundaries = b
        .entries
        .iter()
        .filter(|e| matches!(e.body, TranscriptBody::ContextEpochChanged { .. }))
        .count();
    let epoch_change = epoch != session.context_epoch;
    if epoch_change {
        match &b.entries[0].body {
            TranscriptBody::ContextEpochChanged {
                previous_epoch,
                next_epoch,
                ..
            } if previous_epoch.get() == session.context_epoch
                && next_epoch.get() == epoch
                && session.context_epoch.checked_add(1) == Some(epoch)
                && boundaries == 1 => {}
            _ => refuse!(BatchRefusal::EpochBoundaryRequired {
                current: session.context_epoch,
                presented: epoch
            }),
        }
        if !tx.sessions().pending_calls(&session.id, 1)?.is_empty() {
            refuse!(BatchRefusal::UnresolvedToolCalls)
        }
    } else if boundaries > 0 {
        refuse!(BatchRefusal::DuplicateBoundary)
    }

    let now = tx.now_ms();
    let mut kinds: std::collections::BTreeMap<&'static str, i64> = Default::default();
    // Links to the session's work root are written after the batch event (they carry its
    // sequence); collected here as (kind, entry ID, summary, body).
    let mut pending_links: Vec<(&'static str, Id, String, Value)> = Vec::new();
    // Every completed invocation of every session is a durable fact (written once the batch's
    // committed sequence is known), whether or not the session has a project or work root.
    let mut invocations: Vec<StoredInvocation> = Vec::new();
    for entry in &b.entries {
        let kind = body_kind(&entry.body);
        *kinds.entry(kind).or_default() += 1;
        if tx.sessions().entry_exists(&session.id, &entry.entry_id)? {
            refuse!(BatchRefusal::DuplicateEntry(entry.entry_id.clone()))
        }
        for (what, reference) in references(&entry.body) {
            if !reference_resolves(tx, ctx, session.project_id.as_ref(), reference)? {
                refuse!(BatchRefusal::UnknownReference(what))
            }
        }
        // A committed entry's references are artifact references (same transaction).
        for (_, reference) in references(&entry.body) {
            crate::services::artifacts::reference_from_transcript(
                tx,
                ctx,
                reference,
                &session.id,
                session.root_id.clone(),
            )?;
        }
        let sequence = entry.sequence.get();
        let body = serde_json::to_value(&entry.body)
            .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
        tx.sessions().insert_entry(&StoredEntry {
            session_id: session.id.clone(),
            sequence,
            entry_id: entry.entry_id.clone(),
            batch_id: b.batch_id.clone(),
            context_epoch: epoch,
            actor: actor.clone(),
            kind: kind.to_owned(),
            created_at_ms: now,
            body,
        })?;
        match &entry.body {
            TranscriptBody::ToolCallRecorded { tool_call } => {
                let call = &tool_call.call;
                let ordinal = call.ordinal.get();
                if tx
                    .sessions()
                    .tool_call(&session.id, &call.attempt_id.0, &call.native_call_id)?
                    .is_some()
                    || tx
                        .sessions()
                        .ordinal_taken(&session.id, &call.attempt_id.0, ordinal)?
                {
                    refuse!(BatchRefusal::DuplicateCall)
                }
                tx.sessions().insert_tool_call(&StoredToolCall {
                    session_id: session.id.clone(),
                    attempt_id: call.attempt_id.0.clone(),
                    native_call_id: call.native_call_id.clone(),
                    ordinal,
                    capability_id: tool_call.capability_id.as_str().to_owned(),
                    call_sequence: sequence,
                    response_sequence: None,
                    outcome: None,
                    operation_id: None,
                })?;
                pending_links.push((
                    "tool_call",
                    entry.entry_id.clone(),
                    format!(
                        "Tool call {} (ordinal {ordinal}) at transcript {sequence}",
                        tool_call.capability_id.as_str()
                    ),
                    json!({"sessionId": session.id.as_str(), "sequence": sequence,
                        "attemptId": call.attempt_id.0.as_str(), "nativeCallId": call.native_call_id,
                        "ordinal": ordinal, "capabilityId": tool_call.capability_id.as_str()}),
                ));
            }
            TranscriptBody::ToolResponseRecorded { call, outcome } => {
                let Some(mut recorded) = tx.sessions().tool_call(
                    &session.id,
                    &call.attempt_id.0,
                    &call.native_call_id,
                )?
                else {
                    refuse!(BatchRefusal::MissingCall)
                };
                if recorded.ordinal != call.ordinal.get() {
                    refuse!(BatchRefusal::MissingCall)
                }
                if recorded.response_sequence.is_some() {
                    refuse!(BatchRefusal::SecondResponse)
                }
                let (name, operation) = outcome_kind(outcome);
                if let Some(op) = operation
                    && !operation_in_scope(tx, &session, op)?
                {
                    refuse!(BatchRefusal::UnknownOperation(op.clone()))
                }
                recorded.response_sequence = Some(sequence);
                recorded.outcome = Some(name.to_owned());
                recorded.operation_id = operation.cloned();
                tx.sessions().record_response(&recorded)?;
                let call_entry = tx
                    .sessions()
                    .entries(&session.id, recorded.call_sequence - 1, 1)?
                    .into_iter()
                    .next()
                    .filter(|e| e.sequence == recorded.call_sequence)
                    .ok_or_else(|| StorageError::InvalidRecord("tool call entry missing".into()))?;
                invocations.push(StoredInvocation {
                    session_id: session.id.clone(),
                    response_sequence: sequence,
                    call_sequence: recorded.call_sequence,
                    committed_sequence: 0,
                    project_id: session.project_id.clone(),
                    root_id: session.root_id.clone(),
                    call_entry_id: call_entry.entry_id,
                    capability_id: recorded.capability_id.clone(),
                    outcome: name.to_owned(),
                    error_code: match outcome {
                        ToolOutcome::Failed { code } => Some(code.as_str().to_owned()),
                        _ => None,
                    },
                    operation_id: operation.cloned(),
                    actor: actor.clone(),
                    called_at_ms: call_entry.created_at_ms,
                    responded_at_ms: now,
                });
                pending_links.push((
                    "tool_response",
                    entry.entry_id.clone(),
                    format!(
                        "{name} response to {} (ordinal {}) at transcript {sequence}",
                        recorded.capability_id, recorded.ordinal
                    ),
                    json!({"sessionId": session.id.as_str(), "sequence": sequence,
                        "callSequence": recorded.call_sequence, "outcome": name,
                        "operationId": operation.map(|o| o.0.as_str())}),
                ));
            }
            TranscriptBody::ContextEpochChanged {
                previous_epoch,
                next_epoch,
                reason,
                context,
            } => {
                let reason = match reason {
                    crate::protocol::provider::ContextChangeReason::Compaction => "Compaction",
                    crate::protocol::provider::ContextChangeReason::ProviderChange => {
                        "ProviderChange"
                    }
                    crate::protocol::provider::ContextChangeReason::InstructionAdoption => {
                        "InstructionAdoption"
                    }
                };
                let snapshot = serde_json::to_value(context)
                    .map_err(|e| StorageError::InvalidRecord(e.to_string()))?;
                tx.sessions().insert_epoch(&StoredEpoch {
                    session_id: session.id.clone(),
                    epoch: next_epoch.get(),
                    previous_epoch: previous_epoch.get(),
                    reason: reason.to_owned(),
                    identity: epoch_identity(&session.id, next_epoch.get(), context),
                    context: snapshot,
                    sequence,
                    created_at_ms: now,
                })?;
                pending_links.push((
                    "context_epoch",
                    entry.entry_id.clone(),
                    format!(
                        "Context epoch {} -> {} ({reason}) at transcript {sequence}; the original transcript is kept",
                        previous_epoch.get(),
                        next_epoch.get()
                    ),
                    json!({"sessionId": session.id.as_str(), "sequence": sequence,
                        "previousEpoch": previous_epoch.get(), "nextEpoch": next_epoch.get(), "reason": reason}),
                ));
            }
            TranscriptBody::OperationUpdate {
                operation_id,
                revision,
                ..
            } => {
                if !operation_in_scope(tx, &session, operation_id)? {
                    refuse!(BatchRefusal::UnknownOperation(operation_id.clone()))
                }
                pending_links.push((
                    "operation_update",
                    entry.entry_id.clone(),
                    format!(
                        "Operation {} update (revision {}) at transcript {sequence}",
                        operation_id.0.as_str(),
                        revision.get()
                    ),
                    json!({"sessionId": session.id.as_str(), "sequence": sequence,
                        "operationId": operation_id.0.as_str(), "revision": revision.get()}),
                ));
            }
            _ => {}
        }
    }
    let first = presented;
    let last = first + b.entries.len() as i64 - 1;
    if let Some(checkpoint) = &batch.checkpoint
        && !reference_resolves(tx, ctx, session.project_id.as_ref(), checkpoint)?
    {
        refuse!(BatchRefusal::UnknownReference("checkpoint"))
    }
    for s in &batch.context_sources {
        if let Some(a) = &s.artifact
            && !reference_resolves(tx, ctx, session.project_id.as_ref(), a)?
        {
            refuse!(BatchRefusal::UnknownReference("context_source"))
        }
    }

    session.next_sequence = last + 1;
    if epoch_change || path == AppendPath::UserMessage {
        session.revision += 1;
    }
    session.context_epoch = epoch;
    if batch.checkpoint.is_some() {
        session.checkpoint_sequence = last;
    }
    tx.sessions().update_session(&session)?;

    let event_id = EventId(tx.new_id(IdKind::Event));
    let committed = tx.history().append(NewEvent {
        id: event_id.clone(),
        actor: actor.clone(),
        project_id: session.project_id.clone(),
        root_id: session.root_id.clone(),
        operation_id: None,
        aggregate_kind: SESSION_AGGREGATE.into(),
        aggregate_id: session.id.as_str().to_owned(),
        aggregate_revision: session.revision,
        kind: BATCH_APPENDED.into(),
        payload: json!({
            "batchId": b.batch_id.as_str(),
            "firstSequence": first,
            "lastSequence": last,
            "contextEpoch": epoch,
            "epochChanged": epoch_change,
            "entries": kinds,
            "checkpointSequence": batch.checkpoint.as_ref().map(|_| last),
            "contextSources": batch.context_sources.len(),
            "usage": batch.usage.len(),
            "principalKind": format!("{:?}", ctx.principal_kind()),
            "invocationId": ctx.invocation_id().as_str(),
        }),
    })?;
    let stored = StoredBatch {
        batch_id: b.batch_id.clone(),
        session_id: session.id.clone(),
        payload_digest: digest.to_owned(),
        actor: actor.clone(),
        context_epoch: epoch,
        first_sequence: first,
        last_sequence: last,
        event_id: event_id.0.clone(),
        committed_sequence: committed,
        accepted_at_ms: now,
    };
    tx.sessions().insert_batch(&stored)?;
    for mut invocation in invocations {
        invocation.committed_sequence = committed;
        tx.sessions().insert_invocation(&invocation)?;
        crate::services::usage::ingest::tool_invocation(tx, &invocation)?;
    }
    if let Some(checkpoint) = &batch.checkpoint {
        tx.sessions().insert_checkpoint(&StoredCheckpoint {
            session_id: session.id.clone(),
            sequence: last,
            batch_id: b.batch_id.clone(),
            artifact: serde_json::to_value(checkpoint)
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
            created_at_ms: now,
        })?;
    }
    for (i, s) in batch.context_sources.iter().enumerate() {
        tx.sessions().insert_context_source(&StoredContextSource {
            session_id: session.id.clone(),
            epoch,
            // Ordered within the epoch by recording position: at most 128 sources per batch.
            ordinal: first * 256 + i as i64 + 1,
            source_id: s.source_id.clone(),
            kind: s.kind.clone(),
            source_revision: s.source_revision,
            artifact: s
                .artifact
                .as_ref()
                .map(serde_json::to_value)
                .transpose()
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
            token_estimate: s.token_estimate,
            disposition: s.disposition.as_str().to_owned(),
            // These sources describe the context used for this batch, including its first
            // step. Recording the tail would incorrectly hide them from earlier steps.
            sequence: first,
        })?;
    }
    for (i, u) in batch.usage.iter().enumerate() {
        let usage = StoredUsage {
            session_id: session.id.clone(),
            batch_id: b.batch_id.clone(),
            ordinal: i as i64 + 1,
            project_id: session.project_id.clone(),
            root_id: session.root_id.clone(),
            committed_sequence: committed,
            item: serde_json::to_value(u)
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
        };
        tx.sessions().insert_usage(&usage)?;
        crate::services::usage::ingest::session_usage(tx, &usage)?;
    }
    if let Some(checkpoint) = &batch.checkpoint {
        pending_links.push((
            "checkpoint",
            b.entries[b.entries.len() - 1].entry_id.clone(),
            format!("Checkpoint at transcript {last}"),
            json!({"sessionId": session.id.as_str(), "sequence": last,
                "artifact": serde_json::to_value(checkpoint).unwrap_or(Value::Null)}),
        ));
    }
    if let (Some(root), Some(project)) = (session.root_id.clone(), session.project_id.clone()) {
        let parent = session_link(tx, &session.id)?;
        for (kind, entry_id, summary, body) in pending_links {
            let link = StoredLink {
                id: tx.new_id(IdKind::Link),
                project_id: project.clone(),
                root_id: root.clone(),
                kind: kind.to_owned(),
                target_kind: "transcript_entry".into(),
                target_id: entry_id.as_str().to_owned(),
                parent_id: parent.clone(),
                supersedes_id: None,
                actor: actor.clone(),
                event_id: event_id.0.clone(),
                sequence: committed,
                repository_id: None,
                revision: None,
                summary,
                body,
                created_at_ms: now,
            };
            tx.sessions().insert_link(&link)?;
        }
    }
    Ok(Ok(BatchReceipt::of(&stored)))
}

/// The `session` link of a work session (its lineage parent), if it has one.
pub fn session_link(
    tx: &mut dyn WriteTransaction,
    session: &Id,
) -> Result<Option<Id>, StorageError> {
    Ok(tx
        .sessions()
        .links_to("session", session.as_str(), 1)?
        .into_iter()
        .find(|l| l.kind == "session")
        .map(|l| l.id))
}

/// Whether `operation` exists and belongs to the session's scope: an operation of a work root
/// in the session's project, or one without a root for a workspace session. Another project's
/// operation is reported unknown, so its existence is not disclosed.
pub fn operation_in_scope(
    tx: &mut dyn WriteTransaction,
    session: &StoredSession,
    operation: &OperationId,
) -> Result<bool, StorageError> {
    let Some(op) = tx.operations().operation(operation)? else {
        return Ok(false);
    };
    Ok(match &op.root_id {
        Some(root) => match tx.sessions().work_root(root)? {
            Some(w) => session.project_id.as_ref() == Some(&w.project_id),
            None => false,
        },
        None => session.project_id.is_none(),
    })
}

/// Identity of an epoch's context: the session, epoch number and the immutable snapshot's digest
/// and length. Equal snapshots in different sessions or epochs have different identities. This
/// is not the provider prompt-cache key: that also binds instruction and tool-schema revisions,
/// the route binding and the reusable prefix, and belongs to the router's attempt envelope
/// (`AttemptEnvelope.context_digest`/`cache_epoch`).
pub fn epoch_identity(session: &Id, epoch: i64, context: &ArtifactRef) -> String {
    sha256_hex(
        format!(
            "{}|{epoch}|{}|{}",
            session.as_str(),
            String::from(context.sha256.clone()),
            context.byte_length.as_str()
        )
        .as_bytes(),
    )
}

/// The host runtime's session writer (provider turn assembler, tool broker results, host
/// notices, compaction). It is a host-owned ingress, not a public operation: a package-, MCP- or
/// schedule-supplied transcript is never accepted, and the batch must belong to the session the
/// invocation is bound to. In one unit of work it checks the invocation's store and restore
/// epoch, the principal's registration and current binding (revocation, project), the session
/// owner, then appends.
pub struct SessionWriter<'a> {
    pub store: &'a dyn ApplicationStore,
}
impl SessionWriter<'_> {
    pub fn append(
        &self,
        ctx: &InvocationContext,
        batch: SessionBatch,
        options: WriteOptions,
    ) -> Result<BatchOutcome, StorageError> {
        let ctx = ctx.clone();
        run_write(self.store, options, move |tx| {
            Self::append_in(tx, &ctx, &batch)
        })
    }
    /// Same host authorization and atomic batch append inside an existing fenced unit of work.
    pub fn append_in(
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        batch: &SessionBatch,
    ) -> Result<BatchOutcome, StorageError> {
        let store = tx.store();
        if &store.store_id != ctx.store_id() || store.restore_epoch != ctx.restore_epoch().get() {
            return Ok(BatchOutcome::Refused(BatchRefusal::Unauthorized(
                "StaleInvocation",
            )));
        }
        // A writer may always recover the receipt of its own identical committed batch,
        // even after its session closed or its authority was revoked.
        if let Some(prior) = tx
            .sessions()
            .batch(&batch.batch.session_id, &batch.batch.batch_id)?
            && prior.actor == *ctx.principal()
            && prior.payload_digest == batch_digest(ctx.principal(), batch)?
        {
            return Ok(BatchOutcome::Replayed(BatchReceipt::of(&prior)));
        }
        if let Some(refusal) = writer_refusal(tx, ctx, batch)? {
            return Ok(BatchOutcome::Refused(refusal));
        }
        let actor = ctx.principal().clone();
        append_session_batch(tx, ctx, &actor, batch, AppendPath::Runtime)
    }
}

fn writer_refusal(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    batch: &SessionBatch,
) -> Result<Option<BatchRefusal>, StorageError> {
    if !matches!(
        ctx.principal_kind(),
        PrincipalKind::Agent | PrincipalKind::Host
    ) {
        return Ok(Some(BatchRefusal::Unauthorized("HostOnly")));
    }
    if tx.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind()) {
        return Ok(Some(BatchRefusal::Unauthorized("UnregisteredPrincipal")));
    }
    if ctx.session_id() != Some(&batch.batch.session_id) {
        return Ok(Some(BatchRefusal::Unauthorized("ForeignSession")));
    }
    let Some(session) = tx.sessions().session(&batch.batch.session_id)? else {
        return Ok(Some(BatchRefusal::SessionNotFound));
    };
    if ctx.principal_kind() == PrincipalKind::Agent {
        let Some(binding) = tx.authority().principal(ctx.principal())? else {
            return Ok(Some(BatchRefusal::Unauthorized("UnboundPrincipal")));
        };
        if !binding.active {
            return Ok(Some(BatchRefusal::Unauthorized("PrincipalRevoked")));
        }
        if binding
            .project_id
            .as_ref()
            .is_some_and(|p| session.project_id.as_ref() != Some(p))
        {
            return Ok(Some(BatchRefusal::Unauthorized("OutOfProjectScope")));
        }
        if &session.agent_id != ctx.principal() {
            return Ok(Some(BatchRefusal::Unauthorized("NotSessionAgent")));
        }
    }
    if session.closed() {
        return Ok(Some(BatchRefusal::SessionClosed));
    }
    Ok(None)
}
