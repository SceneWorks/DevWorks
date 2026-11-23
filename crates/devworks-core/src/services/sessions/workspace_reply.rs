//! Durable workspace reply intents, ordered by the human messages that request them.
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::canonical_digest;
use crate::ports::IdKind;
use crate::ports::storage::{
    NewOperation, NewQueueEntry, StorageError, StoredSession, WriteTransaction,
};
use crate::protocol::provider::ConversationRoutePreference;
use crate::protocol::{ActorId, Id, OperationId};
use crate::settings::catalog::AgentRole;
use serde::{Deserialize, Serialize};

pub const CONSUMER: &str = "workspace_reply";
pub const CAPABILITY: &str = "workspace.reply";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplyIntent {
    pub session_id: Id,
    pub agent_id: ActorId,
    pub creator_id: ActorId,
    pub user_entry_id: Id,
    pub user_sequence: i64,
    pub context_epoch: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<ConversationRoutePreference>,
}

/// An older Home chat can acquire the configured agent only from its recorded human creator.
/// Ownership, the triggering message and its reply intent all change in one writer transaction.
pub(super) fn prepare(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    session: &StoredSession,
    coordinator: &ActorId,
) -> Result<bool, StorageError> {
    if ctx.principal_kind() != PrincipalKind::Human
        || ctx.principal() != &session.created_by
        || session.project_id.is_some()
        || session.root_id.is_some()
        || session.closed()
        || !tx
            .authority()
            .principal(coordinator)?
            .is_some_and(|binding| {
                binding.active
                    && binding.kind == PrincipalKind::Agent
                    && binding.role == Some(AgentRole::WorkspaceCoordinator)
                    && binding.project_id.is_none()
            })
    {
        return Ok(false);
    }
    if &session.agent_id == coordinator {
        return Ok(true);
    }
    if session.agent_id != session.created_by {
        return Ok(false);
    }
    tx.sessions().adopt_workspace_coordinator(
        &session.id,
        session.revision,
        &session.created_by,
        coordinator,
    )
}

pub(super) fn record(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    session: &StoredSession,
    entry_id: Id,
    sequence: i64,
    route: Option<ConversationRoutePreference>,
) -> Result<OperationId, StorageError> {
    let intent = ReplyIntent {
        session_id: session.id.clone(),
        agent_id: session.agent_id.clone(),
        creator_id: session.created_by.clone(),
        user_entry_id: entry_id,
        user_sequence: sequence,
        context_epoch: session.context_epoch,
        route,
    };
    let payload = serde_json::to_value(&intent)
        .map_err(|error| StorageError::InvalidRecord(error.to_string()))?;
    let args_digest = canonical_digest(&payload)
        .map_err(|error| StorageError::InvalidRecord(format!("reply intent digest: {error:?}")))?;
    let operation_id = OperationId(tx.new_id(IdKind::Operation));
    tx.operations().record_intent(NewOperation {
        id: operation_id.clone(),
        root_id: None,
        assignment_id: None,
        capability: CAPABILITY.into(),
        schema_version: 1,
        args_digest,
        payload: payload.clone(),
    })?;
    let (event_id, _) =
        super::session_event(tx, ctx, session, "WorkspaceReplyRequested", payload.clone())?;
    let entry = NewQueueEntry {
        id: tx.new_id(IdKind::QueueEntry),
        consumer: CONSUMER.into(),
        event_id,
        root_id: None,
        operation_id: Some(operation_id.clone()),
        partition: Some(session.id.as_str().into()),
        priority: 0,
        ready_at_ms: tx.now_ms(),
        payload,
    };
    tx.queue().enqueue(entry)?;
    Ok(operation_id)
}
