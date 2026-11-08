//! The retained work-message inbox's single transactional delivery pass. A gateway command can
//! long-poll by repeating this pass between transactions; no connection is held during a wait.
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::storage::{
    StorageError, StoredAgentDelivery, StoredAgentMessage, StoredAgentMessageEvent,
    WriteTransaction,
};
use crate::protocol::{AssignmentId, Id, ProjectId, Revision, RevisionDimension};
use std::collections::HashSet;

mod post;
pub use post::{MessageKind, PostSpec, PostedMessage, post_message};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recipient {
    Session(Id),
    Human,
}

#[derive(Clone, Debug)]
pub struct DeliveredInbox {
    pub items: Vec<(StoredAgentDelivery, StoredAgentMessage)>,
    /// The last delivery sequence returned, or the supplied position when the page is empty.
    pub next_sequence: i64,
}

pub enum AckSelection {
    Messages(Vec<Id>),
    Through(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    Answered,
    Declined,
}

impl Resolution {
    fn state(self) -> &'static str {
        match self {
            Self::Answered => "Answered",
            Self::Declined => "Declined",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Acknowledged {
    pub changed: Vec<(StoredAgentDelivery, StoredAgentMessage)>,
}

fn authorize_recipient(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    project: &ProjectId,
    recipient: &Recipient,
) -> Result<(), StorageError> {
    if tx.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind()) {
        return Err(StorageError::ScopeDenied);
    }
    if ctx.project_id().is_some_and(|scope| scope != project) {
        return Err(StorageError::ScopeDenied);
    }
    match recipient {
        Recipient::Session(id) => {
            if ctx.principal_kind() != PrincipalKind::Agent || ctx.session_id() != Some(id) {
                return Err(StorageError::ScopeDenied);
            }
            let session = tx
                .sessions()
                .session(id)?
                .ok_or(StorageError::ScopeDenied)?;
            let root_project = match &session.root_id {
                Some(root) => tx.sessions().work_root(root)?.map(|work| work.project_id),
                None => None,
            };
            if session.project_id.is_some()
                && root_project.is_some()
                && session.project_id != root_project
            {
                return Err(StorageError::ScopeDenied);
            }
            let session_project = session.project_id.clone().or(root_project);
            if session.closed()
                || session_project.as_ref() != Some(project)
                || session.agent_id != *ctx.principal()
                || ctx
                    .root_id()
                    .is_some_and(|root| session.root_id.as_ref() != Some(root))
            {
                return Err(StorageError::ScopeDenied);
            }
        }
        Recipient::Human => {
            if ctx.principal_kind() != PrincipalKind::Human || ctx.session_id().is_some() {
                return Err(StorageError::ScopeDenied);
            }
            if tx.projects().project(project)?.is_none() {
                return Err(StorageError::ScopeDenied);
            }
        }
    }
    Ok(())
}

fn delivery_for(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
    recipient: &Recipient,
    message_id: &Id,
) -> Result<(StoredAgentDelivery, StoredAgentMessage), StorageError> {
    let delivery = match recipient {
        Recipient::Session(id) => tx.runs().agent_session_delivery(message_id, id)?,
        Recipient::Human => tx.runs().agent_delivery(message_id, None)?,
    }
    .ok_or(StorageError::ScopeDenied)?;
    if delivery.project_id != *project
        || delivery.recipient_human != matches!(recipient, Recipient::Human)
        || delivery.recipient_session_id.as_ref()
            != match recipient {
                Recipient::Session(id) => Some(id),
                Recipient::Human => None,
            }
    {
        return Err(StorageError::ScopeDenied);
    }
    let message = tx
        .runs()
        .agent_message(message_id)?
        .ok_or(StorageError::ScopeDenied)?;
    Ok((delivery, message))
}

/// Returns pending and delivered copies, marking pending ones Delivered in the same write unit.
/// An unacknowledged copy stays visible on replay after a lost reply; only `ack` makes it Read.
pub fn deliver_inbox(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    project: &ProjectId,
    recipient: &Recipient,
    after: i64,
    include_read: bool,
    limit: u16,
) -> Result<DeliveredInbox, StorageError> {
    if after < 0 || limit == 0 || limit > 100 {
        return Err(StorageError::InvalidRecord(
            "invalid inbox cursor or limit".into(),
        ));
    }
    if after > tx.runs().agent_delivery_head()? {
        return Err(StorageError::CursorExpired);
    }
    authorize_recipient(tx, ctx, project, recipient)?;
    let mut rows = match recipient {
        Recipient::Session(id) => {
            tx.runs()
                .agent_session_inbox(project, id, after, include_read, limit)?
        }
        Recipient::Human => tx
            .runs()
            .agent_inbox(project, None, after, include_read, limit)?,
    };
    let now = tx.now_ms();
    for (delivery, _) in &mut rows {
        if delivery.state == "Pending" {
            let expected = delivery.revision;
            delivery.state = "Delivered".into();
            delivery.revision += 1;
            delivery.delivered_at_ms = Some(now);
            tx.runs().update_agent_delivery(delivery, expected)?;
        }
    }
    Ok(DeliveredInbox {
        next_sequence: rows.last().map_or(after, |(delivery, _)| delivery.sequence),
        items: rows,
    })
}

/// Rebinds open item questions/requests to a newly owning session without rewriting older
/// recipients. A retry finds its existing copy and leaves the event stream unchanged.
pub fn handover_item(
    tx: &mut dyn WriteTransaction,
    assignment_id: &AssignmentId,
) -> Result<usize, StorageError> {
    let assignment = tx
        .runs()
        .assignment(assignment_id)?
        .ok_or(StorageError::ScopeDenied)?;
    if !assignment.state.is_live() {
        return Err(StorageError::ScopeDenied);
    }
    let item = &assignment.work_id;
    let session_id = &assignment.session_id;
    let work = tx
        .sessions()
        .work_root(item)?
        .ok_or(StorageError::ScopeDenied)?;
    let session = tx
        .sessions()
        .session(session_id)?
        .ok_or(StorageError::ScopeDenied)?;
    if session.closed() || post::session_project(tx, &session)?.as_ref() != Some(&work.project_id) {
        return Err(StorageError::ScopeDenied);
    }
    let mut before: Option<Id> = None;
    let mut handed_over = 0;
    loop {
        let page = tx
            .runs()
            .agent_messages(&work.project_id, before.as_ref(), 500)?;
        if page.is_empty() {
            break;
        }
        before = page.last().map(|message| message.id.clone());
        let page_len = page.len();
        for message in page {
            if message.target_item_id.as_ref() != Some(item)
                || message.state.as_deref() != Some("Open")
                || !matches!(message.message_class.as_str(), "Question" | "Request")
                || message.to_human
                || message.sender_session_id.as_ref() == Some(session_id)
                || tx
                    .runs()
                    .agent_session_delivery(&message.id, session_id)?
                    .is_some()
            {
                continue;
            }
            let now = tx.now_ms();
            let delivery = StoredAgentDelivery {
                sequence: tx.runs().agent_delivery_head()? + 1,
                message_id: message.id.clone(),
                project_id: work.project_id.clone(),
                recipient_assignment_id: None,
                recipient_human: false,
                state: "Pending".into(),
                revision: 1,
                delivered_at_ms: None,
                read_at_ms: None,
                recipient_session_id: Some(session_id.clone()),
                handed_over: true,
                resolved_at_ms: None,
            };
            tx.runs().insert_agent_delivery(&delivery)?;
            let event_sequence = tx.runs().agent_event_head()? + 1;
            tx.runs()
                .insert_agent_message_event(&StoredAgentMessageEvent {
                    sequence: event_sequence,
                    project_id: work.project_id.clone(),
                    message_id: message.id,
                    kind: "HandedOver".into(),
                    actor_id: session.agent_id.clone(),
                    recipient_assignment_id: None,
                    recipient_session_id: Some(session_id.clone()),
                    recipient_human: false,
                    created_at_ms: now,
                })?;
            handed_over += 1;
        }
        if page_len < 500 {
            break;
        }
    }
    Ok(handed_over)
}

/// Atomically marks the named or cursor-bounded inbox deliveries Read. Repeated acknowledgments
/// are no-ops; the caller must supply a cursor from this inbox when using `Through`.
pub fn acknowledge(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    project: &ProjectId,
    recipient: &Recipient,
    selection: AckSelection,
    resolution: Option<Resolution>,
) -> Result<Acknowledged, StorageError> {
    if resolution.is_some() && !matches!(selection, AckSelection::Messages(_)) {
        return Err(StorageError::InvalidRecord(
            "resolution requires named messages".into(),
        ));
    }
    authorize_recipient(tx, ctx, project, recipient)?;
    let ids = match selection {
        AckSelection::Messages(ids) => {
            if ids.is_empty() || ids.len() > 100 {
                return Err(StorageError::InvalidRecord(
                    "ack message count must be 1 to 100".into(),
                ));
            }
            let mut seen = HashSet::new();
            ids.into_iter()
                .filter(|id| seen.insert(id.clone()))
                .collect::<Vec<_>>()
        }
        AckSelection::Through(up_to) => {
            if up_to < 0 || up_to > tx.runs().agent_delivery_head()? {
                return Err(StorageError::CursorExpired);
            }
            let mut ids = Vec::new();
            let mut after = 0;
            loop {
                let page = match recipient {
                    Recipient::Session(id) => tx
                        .runs()
                        .agent_session_inbox(project, id, after, false, 100)?,
                    Recipient::Human => tx.runs().agent_inbox(project, None, after, false, 100)?,
                };
                if page.is_empty() {
                    break;
                }
                for (delivery, _) in &page {
                    if delivery.sequence > up_to {
                        break;
                    }
                    ids.push(delivery.message_id.clone());
                }
                let last = page.last().expect("nonempty page").0.sequence;
                if last >= up_to {
                    break;
                }
                after = last;
            }
            ids
        }
    };
    let now = tx.now_ms();
    let mut changed = Vec::new();
    for id in ids {
        let (mut delivery, message) = delivery_for(tx, project, recipient, &id)?;
        if !matches!(delivery.state.as_str(), "Read" | "Answered" | "Declined") {
            let expected = delivery.revision;
            delivery.state = "Read".into();
            delivery.revision += 1;
            delivery.delivered_at_ms.get_or_insert(now);
            delivery.read_at_ms = Some(now);
            tx.runs().update_agent_delivery(&delivery, expected)?;
            let event_sequence = tx.runs().agent_event_head()? + 1;
            tx.runs()
                .insert_agent_message_event(&StoredAgentMessageEvent {
                    sequence: event_sequence,
                    project_id: project.clone(),
                    message_id: id,
                    kind: "Read".into(),
                    actor_id: ctx.principal().clone(),
                    recipient_assignment_id: None,
                    recipient_session_id: match recipient {
                        Recipient::Session(id) => Some(id.clone()),
                        Recipient::Human => None,
                    },
                    recipient_human: matches!(recipient, Recipient::Human),
                    created_at_ms: now,
                })?;
            changed.push((delivery, message.clone()));
        }
        if let Some(resolution) = resolution
            && let Some(updated) =
                resolve_message(tx, ctx, project, recipient, &message, resolution)?
        {
            if let Some(entry) = changed.iter_mut().find(|(_, row)| row.id == message.id) {
                *entry = updated;
            } else {
                changed.push(updated);
            }
        }
    }
    Ok(Acknowledged { changed })
}

pub(super) fn resolve_message(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    project: &ProjectId,
    recipient: &Recipient,
    message: &StoredAgentMessage,
    resolution: Resolution,
) -> Result<Option<(StoredAgentDelivery, StoredAgentMessage)>, StorageError> {
    if !matches!(message.message_class.as_str(), "Question" | "Request") {
        return Err(StorageError::InvalidRecord(
            "only a question or request can be resolved".into(),
        ));
    }
    if message.state.as_deref() == Some(resolution.state())
        && message.resolved_by_actor_id.as_ref() == Some(ctx.principal())
        && message.resolved_by_session_id.as_ref() == ctx.session_id()
    {
        return Ok(None);
    }
    if message.state.as_deref() != Some("Open") {
        return Err(StorageError::conflict(
            RevisionDimension::Store,
            "agent_message",
            message.id.clone(),
            Revision::new((message.revision - 1).max(1)).expect("positive revision"),
            Revision::new(message.revision).expect("positive revision"),
        ));
    }
    let now = tx.now_ms();
    let mut resolved = message.clone();
    resolved.state = Some(resolution.state().into());
    resolved.revision += 1;
    resolved.resolved_at_ms = Some(now);
    resolved.resolved_by_actor_id = Some(ctx.principal().clone());
    resolved.resolved_by_session_id = ctx.session_id().cloned();
    tx.runs()
        .resolve_agent_message(&resolved, message.revision)?;
    let mut delivery = match recipient {
        Recipient::Session(id) => tx.runs().agent_session_delivery(&message.id, id)?,
        Recipient::Human => tx.runs().agent_delivery(&message.id, None)?,
    };
    if delivery.is_none() {
        let new = StoredAgentDelivery {
            sequence: tx.runs().agent_delivery_head()? + 1,
            message_id: message.id.clone(),
            project_id: project.clone(),
            recipient_assignment_id: None,
            recipient_human: matches!(recipient, Recipient::Human),
            state: "Pending".into(),
            revision: 1,
            delivered_at_ms: None,
            read_at_ms: None,
            recipient_session_id: match recipient {
                Recipient::Session(id) => Some(id.clone()),
                Recipient::Human => None,
            },
            handed_over: false,
            resolved_at_ms: None,
        };
        tx.runs().insert_agent_delivery(&new)?;
        delivery = Some(new);
    }
    let mut delivery = delivery.expect("inserted or existing delivery");
    let expected = delivery.revision;
    delivery.state = resolution.state().into();
    delivery.revision += 1;
    delivery.delivered_at_ms.get_or_insert(now);
    delivery.read_at_ms.get_or_insert(now);
    delivery.resolved_at_ms = Some(now);
    tx.runs().update_agent_delivery(&delivery, expected)?;
    let event_sequence = tx.runs().agent_event_head()? + 1;
    tx.runs()
        .insert_agent_message_event(&StoredAgentMessageEvent {
            sequence: event_sequence,
            project_id: project.clone(),
            message_id: message.id.clone(),
            kind: resolution.state().into(),
            actor_id: ctx.principal().clone(),
            recipient_assignment_id: None,
            recipient_session_id: match recipient {
                Recipient::Session(id) => Some(id.clone()),
                Recipient::Human => None,
            },
            recipient_human: matches!(recipient, Recipient::Human),
            created_at_ms: now,
        })?;
    Ok(Some((delivery, resolved)))
}
