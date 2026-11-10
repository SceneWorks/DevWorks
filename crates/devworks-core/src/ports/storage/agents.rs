//! Durable agent messages and per-recipient delivery state. The message body and attribution
//! remain immutable; delivery and question resolution advance by guarded revisions.
use crate::protocol::{ActorId, AssignmentId, Id, ProjectId, RootId};
use serde_json::Value;

/// The approved role text and task brief an assignment was launched with.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredAgentAssignmentContext {
    pub assignment_id: AssignmentId,
    pub project_id: ProjectId,
    pub role_id: Id,
    pub role_instructions: String,
    pub role_revisions: Value,
    pub brief: String,
    pub task_ids: Value,
    pub source_checkpoint_id: Option<Id>,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredAgentMessage {
    pub id: Id,
    pub project_id: ProjectId,
    pub root_id: Option<RootId>,
    pub channel: String,
    pub from_assignment_id: Option<AssignmentId>,
    pub to_assignment_id: Option<AssignmentId>,
    pub message_class: String,
    pub body: String,
    pub attachments: Value,
    pub in_reply_to_id: Option<Id>,
    pub state: Option<String>,
    pub blocking: bool,
    pub idempotency_key: Option<String>,
    pub revision: i64,
    pub created_at_ms: i64,
    pub resolved_at_ms: Option<i64>,
    pub from_actor_id: ActorId,
    pub target_item_id: Option<RootId>,
    pub target_feature_id: Option<RootId>,
    pub resource_name: Option<String>,
    pub target_session_id: Option<Id>,
    pub to_human: bool,
    pub sender_session_id: Option<Id>,
    pub resolved_by_actor_id: Option<ActorId>,
    pub resolved_by_session_id: Option<Id>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredAgentDelivery {
    pub sequence: i64,
    pub message_id: Id,
    pub project_id: ProjectId,
    pub recipient_assignment_id: Option<AssignmentId>,
    pub recipient_human: bool,
    pub state: String,
    pub revision: i64,
    pub delivered_at_ms: Option<i64>,
    pub read_at_ms: Option<i64>,
    pub recipient_session_id: Option<Id>,
    pub handed_over: bool,
    /// The one successor that inherited this recipient copy; never set on the inherited copy.
    pub handed_to_session_id: Option<Id>,
    pub resolved_at_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredAgentMessageEvent {
    pub sequence: i64,
    pub project_id: ProjectId,
    pub message_id: Id,
    pub kind: String,
    pub actor_id: ActorId,
    pub recipient_assignment_id: Option<AssignmentId>,
    pub recipient_session_id: Option<Id>,
    pub recipient_human: bool,
    pub created_at_ms: i64,
}

/// Filters the retained project message history in descending Posted-event sequence.
pub struct AgentMessageHistoryFilter<'a> {
    pub project: &'a ProjectId,
    pub channel: Option<&'a str>,
    pub item: Option<&'a RootId>,
    pub feature: Option<&'a RootId>,
    pub resource: Option<&'a str>,
    pub open_only: bool,
    pub to_human: bool,
    pub before: Option<i64>,
    pub limit: u16,
}
