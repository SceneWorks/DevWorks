//! The retained work-message inbox's single transactional delivery pass. A gateway command can
//! long-poll by repeating this pass between transactions; no connection is held during a wait.
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::storage::{
    StorageError, StoredAgentDelivery, StoredAgentMessage, StoredAgentMessageEvent,
    WriteTransaction,
};
use crate::protocol::{Id, ProjectId};
use std::collections::HashSet;

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

/// Atomically marks the named or cursor-bounded inbox deliveries Read. Repeated acknowledgments
/// are no-ops; the caller must supply a cursor from this inbox when using `Through`.
pub fn acknowledge(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    project: &ProjectId,
    recipient: &Recipient,
    selection: AckSelection,
) -> Result<Acknowledged, StorageError> {
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
        if matches!(delivery.state.as_str(), "Read" | "Answered" | "Declined") {
            continue;
        }
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
        changed.push((delivery, message));
    }
    Ok(Acknowledged { changed })
}
