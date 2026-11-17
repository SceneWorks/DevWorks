//! The one place agent-message rows are built and written. The assignment inbox (`agents.messages`,
//! `agents.inbox`, `agents.acknowledge`) and the retained inbox (`agents.post`, `agents.receive`,
//! `agents.ack`) store the same message, delivery and event rows; each keeps its own cursor and
//! reply, but neither assembles a row on its own.
use crate::ports::IdKind;
use crate::ports::storage::{
    StorageError, StoredAgentDelivery, StoredAgentMessage, StoredAgentMessageEvent,
    WriteTransaction,
};
use crate::protocol::{ActorId, AssignmentId, Id, ProjectId};

/// Who a delivery or an event concerns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    None,
    Assignment(AssignmentId),
    Session(Id),
    Human,
}

impl Target {
    fn assignment(&self) -> Option<AssignmentId> {
        match self {
            Self::Assignment(id) => Some(id.clone()),
            _ => None,
        }
    }
    fn session(&self) -> Option<Id> {
        match self {
            Self::Session(id) => Some(id.clone()),
            _ => None,
        }
    }
}

impl From<&super::retained::Recipient> for Target {
    fn from(recipient: &super::retained::Recipient) -> Self {
        match recipient {
            super::retained::Recipient::Session(id) => Self::Session(id.clone()),
            super::retained::Recipient::Human => Self::Human,
        }
    }
}

/// What every message row needs; the caller sets the rest with struct update syntax.
pub(crate) struct NewMessage<'a> {
    pub project: &'a ProjectId,
    pub channel: &'static str,
    pub class: &'a str,
    pub body: &'a str,
    pub from_actor: &'a ActorId,
}

/// A message row with every optional field empty, stamped with a new id and the transaction time.
pub(crate) fn message(tx: &mut dyn WriteTransaction, spec: NewMessage<'_>) -> StoredAgentMessage {
    StoredAgentMessage {
        id: tx.new_id(IdKind::Event),
        project_id: spec.project.clone(),
        root_id: None,
        channel: spec.channel.into(),
        from_assignment_id: None,
        to_assignment_id: None,
        message_class: spec.class.into(),
        body: spec.body.into(),
        attachments: serde_json::json!([]),
        in_reply_to_id: None,
        state: None,
        blocking: false,
        idempotency_key: None,
        revision: 1,
        created_at_ms: tx.now_ms(),
        resolved_at_ms: None,
        from_actor_id: spec.from_actor.clone(),
        target_item_id: None,
        target_feature_id: None,
        resource_name: None,
        target_session_id: None,
        to_human: false,
        sender_session_id: None,
        resolved_by_actor_id: None,
        resolved_by_session_id: None,
    }
}

/// Inserts one Pending delivery copy at the next delivery sequence.
pub(crate) fn insert_delivery(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
    message_id: &Id,
    target: &Target,
    handed_over: bool,
) -> Result<StoredAgentDelivery, StorageError> {
    let delivery = StoredAgentDelivery {
        sequence: tx.runs().agent_delivery_head()? + 1,
        message_id: message_id.clone(),
        project_id: project.clone(),
        recipient_assignment_id: target.assignment(),
        recipient_human: *target == Target::Human,
        state: "Pending".into(),
        revision: 1,
        delivered_at_ms: None,
        read_at_ms: None,
        recipient_session_id: target.session(),
        handed_over,
        handed_to_session_id: None,
        resolved_at_ms: None,
    };
    tx.runs().insert_agent_delivery(&delivery)?;
    Ok(delivery)
}

/// Appends one event at the next event sequence.
pub(crate) fn append_event(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
    message_id: &Id,
    kind: &str,
    actor: &ActorId,
    target: &Target,
    created_at_ms: i64,
) -> Result<(), StorageError> {
    let sequence = tx.runs().agent_event_head()? + 1;
    tx.runs()
        .insert_agent_message_event(&StoredAgentMessageEvent {
            sequence,
            project_id: project.clone(),
            message_id: message_id.clone(),
            kind: kind.into(),
            actor_id: actor.clone(),
            recipient_assignment_id: target.assignment(),
            recipient_session_id: target.session(),
            recipient_human: *target == Target::Human,
            created_at_ms,
        })
}

/// Whether a delivery has already been read, answered or declined.
pub(crate) fn is_read(delivery: &StoredAgentDelivery) -> bool {
    matches!(delivery.state.as_str(), "Read" | "Answered" | "Declined")
}

/// Marks a delivery Read at `now` and stores it; the caller appends the `Read` event.
pub(crate) fn mark_read(
    tx: &mut dyn WriteTransaction,
    delivery: &mut StoredAgentDelivery,
    now: i64,
) -> Result<(), StorageError> {
    let expected = delivery.revision;
    delivery.state = "Read".into();
    delivery.revision += 1;
    delivery.delivered_at_ms.get_or_insert(now);
    delivery.read_at_ms = Some(now);
    tx.runs().update_agent_delivery(delivery, expected)
}
