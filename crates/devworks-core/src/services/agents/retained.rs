//! The retained work-message inbox's single transactional delivery pass. A gateway command can
//! long-poll by repeating this pass between transactions; no connection is held during a wait.
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::storage::{
    ANY_QUEUE, AgentMessageHistoryFilter, ReadSnapshot, StorageError, StoredAgentDelivery,
    StoredAgentMessage, StoredAgentMessageEvent, WriteTransaction,
};
use crate::protocol::{AssignmentId, Id, ProjectId, Revision, RevisionDimension, RootId};
use base64::Engine as _;
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

/// The retained source's inbox cursor is a base64url `ib:<sequence>` token. It is an
/// observation position, never authority; every read and acknowledgment rechecks the current
/// recipient and project before using it.
pub fn inbox_cursor(sequence: i64) -> Result<String, StorageError> {
    if sequence < 0 {
        return Err(StorageError::InvalidRecord("negative inbox cursor".into()));
    }
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("ib:{sequence}")))
}

pub fn parse_inbox_cursor(cursor: &str) -> Result<i64, StorageError> {
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor.trim())
        .map_err(|_| StorageError::InvalidRecord("not an inbox cursor".into()))?;
    let value = std::str::from_utf8(&decoded)
        .ok()
        .and_then(|text| text.strip_prefix("ib:"))
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or_else(|| StorageError::InvalidRecord("not an inbox cursor".into()))?;
    Ok(value)
}

pub struct MessageHistoryQuery {
    pub project: ProjectId,
    pub channel: Option<String>,
    pub item: Option<crate::protocol::RootId>,
    pub feature: Option<crate::protocol::RootId>,
    pub resource: Option<String>,
    pub open_only: bool,
    pub to_human: bool,
    pub before: Option<i64>,
    pub limit: u16,
}

pub struct MessageHistoryPage {
    pub items: Vec<StoredAgentMessage>,
    pub next_before: Option<i64>,
}

pub struct MessageEventPage {
    pub items: Vec<StoredAgentMessageEvent>,
    pub next_sequence: i64,
}

/// The native UI's cursor feed starts at the current head when no cursor is supplied, matching
/// a new event-stream subscription; a supplied cursor replays later committed events.
pub fn events(
    snapshot: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    project: &ProjectId,
    after: Option<i64>,
    limit: u16,
) -> Result<MessageEventPage, StorageError> {
    if limit == 0 || limit > 100 || after.is_some_and(|cursor| cursor < 0) {
        return Err(StorageError::InvalidRecord(
            "invalid message event cursor or limit".into(),
        ));
    }
    if ctx.principal_kind() != PrincipalKind::Human
        || ctx.session_id().is_some()
        || ctx.project_id().is_some_and(|scope| scope != project)
        || snapshot.actors().actor_kind(ctx.principal())? != Some(PrincipalKind::Human)
        || !snapshot.sessions().project_exists(project)?
    {
        return Err(StorageError::ScopeDenied);
    }
    let head = snapshot.runs().agent_event_head()?;
    let position = after.unwrap_or(head);
    if position > head {
        return Err(StorageError::CursorExpired);
    }
    let items = snapshot
        .runs()
        .agent_message_events(project, position, limit)?;
    let next_sequence = items.last().map_or(position, |event| event.sequence);
    Ok(MessageEventPage {
        items,
        next_sequence,
    })
}

/// Read-only project history. The posted-event sequence is the message cursor; intervening
/// delivery/read events leave gaps but cannot reorder messages.
pub fn history(
    snapshot: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    query: &MessageHistoryQuery,
) -> Result<MessageHistoryPage, StorageError> {
    if query.limit == 0
        || query.limit > 200
        || query.before.is_some_and(|n| n < 0)
        || query.channel.as_deref().is_some_and(|channel| {
            !matches!(
                channel,
                "Item" | "Project" | "Feature" | "Resource" | "Session" | "Human"
            )
        })
        || query.resource.as_deref().is_some_and(str::is_empty)
    {
        return Err(StorageError::InvalidRecord(
            "invalid message history filter".into(),
        ));
    }
    if ctx.principal_kind() != PrincipalKind::Human
        || ctx.session_id().is_some()
        || ctx
            .project_id()
            .is_some_and(|scope| scope != &query.project)
        || snapshot.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
    {
        return Err(StorageError::ScopeDenied);
    }
    if !snapshot.sessions().project_exists(&query.project)? {
        return Err(StorageError::ScopeDenied);
    }
    for (target, kind) in [(query.item.as_ref(), false), (query.feature.as_ref(), true)] {
        if let Some(target) = target {
            let work = snapshot
                .sessions()
                .work_root(target)?
                .ok_or(StorageError::ScopeDenied)?;
            if work.project_id != query.project || (work.kind == "feature") != kind {
                return Err(StorageError::ScopeDenied);
            }
        }
    }
    let mut rows = snapshot
        .runs()
        .agent_message_history(&AgentMessageHistoryFilter {
            project: &query.project,
            channel: query.channel.as_deref(),
            item: query.item.as_ref(),
            feature: query.feature.as_ref(),
            resource: query.resource.as_deref(),
            open_only: query.open_only,
            to_human: query.to_human,
            before: query.before,
            limit: query.limit + 1,
        })?;
    let more = rows.len() > usize::from(query.limit);
    rows.truncate(usize::from(query.limit));
    let next_before = if more {
        rows.last().map(|(sequence, _)| *sequence)
    } else {
        None
    };
    Ok(MessageHistoryPage {
        items: rows.into_iter().map(|(_, message)| message).collect(),
        next_before,
    })
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
    pub item_versions: Vec<ItemVersion>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemVersion {
    pub key: Id,
    pub version: i64,
}

/// A message changes work's derived awaiting-human state only at the first open blocker or
/// last resolved blocker. Keep the item version, queue cursor and work history in that unit.
pub(super) fn awaiting_human_change(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    item: &RootId,
    before: bool,
) -> Result<Option<ItemVersion>, StorageError> {
    let after = tx.runs().open_blocking_question(item)?;
    if before == after {
        return Ok(None);
    }
    let mut work = tx.work().work(&item.0)?.ok_or(StorageError::ScopeDenied)?;
    let statuses = tx.work().statuses(&work.project_id)?;
    let finished = work
        .status_id
        .as_ref()
        .and_then(|id| crate::services::work::workflow::semantic_of(&statuses, id));
    if matches!(
        finished,
        Some(crate::services::work::workflow::DONE | crate::services::work::workflow::CLOSED)
    ) {
        return Ok(None);
    }
    let previous = work.revision;
    work.revision += 1;
    work.updated_at_ms = tx.now_ms();
    work.updated_by = Some(ctx.principal().clone());
    tx.work().update_work(&work, previous)?;
    tx.work().bump_queue(&work.project_id, &work.queue())?;
    tx.work().bump_queue(&work.project_id, ANY_QUEUE)?;
    if after {
        crate::services::work::allocation::invalidate_work(tx, ctx, &work.id)?;
    }
    crate::services::work::event(
        tx,
        ctx,
        &work,
        "AwaitingHumanChanged",
        serde_json::json!({"before": before, "after": after}),
    )?;
    Ok(Some(ItemVersion {
        key: work.id,
        version: work.revision,
    }))
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
    let now = tx.now_ms();
    if let Recipient::Session(id) = recipient {
        tx.sessions().touch_session_activity(id, now)?;
    }
    let mut rows = match recipient {
        Recipient::Session(id) => {
            tx.runs()
                .agent_session_inbox(project, id, after, include_read, limit)?
        }
        Recipient::Human => tx
            .runs()
            .agent_inbox(project, None, after, include_read, limit)?,
    };
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
                handed_to_session_id: None,
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

const DIRECT_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const SILENT_SESSION_MS: i64 = 15 * 60 * 1_000;

/// Passes an unread or still-open itemless direct message once from an ended or silent holder to
/// a new session with the same project and repository scope. The original delivery is immutable
/// to its recipient except for the successor marker; the new copy has its own inbox sequence.
pub fn handover_direct(
    tx: &mut dyn WriteTransaction,
    session_id: &Id,
) -> Result<usize, StorageError> {
    let session = tx
        .sessions()
        .session(session_id)?
        .ok_or(StorageError::ScopeDenied)?;
    if session.closed() {
        return Err(StorageError::ScopeDenied);
    }
    let project = post::session_project(tx, &session)?.ok_or(StorageError::ScopeDenied)?;
    let repositories = tx.sessions().session_repository_scope(session_id)?;
    let now = tx.now_ms();
    let retention_start = now.saturating_sub(DIRECT_RETENTION_MS);
    let stale_before = now.saturating_sub(SILENT_SESSION_MS);
    let mut after = 0;
    let mut inherited = 0;
    loop {
        let page = tx.runs().unhanded_direct_deliveries(&project, after, 500)?;
        if page.is_empty() {
            break;
        }
        let page_len = page.len();
        after = page.last().expect("nonempty page").0.sequence;
        for (mut source, message) in page {
            let Some(holder_id) = source.recipient_session_id.as_ref() else {
                continue;
            };
            let Some(original_id) = message.target_session_id.as_ref() else {
                continue;
            };
            if holder_id == session_id
                || original_id == session_id
                || (source.read_at_ms.is_some() && message.state.as_deref() != Some("Open"))
                || tx
                    .runs()
                    .agent_session_delivery(&message.id, session_id)?
                    .is_some()
            {
                continue;
            }
            let holder = tx
                .sessions()
                .session(holder_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let original = tx
                .sessions()
                .session(original_id)?
                .ok_or(StorageError::ScopeDenied)?;
            if post::session_project(tx, &holder)?.as_ref() != Some(&project)
                || post::session_project(tx, &original)?.as_ref() != Some(&project)
                || tx.sessions().session_repository_scope(original_id)? != repositories
            {
                continue;
            }
            let holder_last = tx
                .sessions()
                .last_entry_at_ms(holder_id)?
                .into_iter()
                .chain(tx.sessions().session_activity_at_ms(holder_id)?)
                .chain(tx.runs().agent_session_activity_at(holder_id)?)
                .chain(std::iter::once(holder.created_at_ms))
                .max()
                .expect("created session has an activity timestamp");
            if !holder.closed()
                && (holder_last > stale_before || post::active_assignment(tx, &holder)?.is_some())
            {
                continue;
            }
            let original_last = if let Some(closed) = original.closed_at_ms {
                closed
            } else {
                tx.sessions()
                    .last_entry_at_ms(original_id)?
                    .into_iter()
                    .chain(tx.sessions().session_activity_at_ms(original_id)?)
                    .chain(tx.runs().agent_session_activity_at(original_id)?)
                    .chain(std::iter::once(original.created_at_ms))
                    .max()
                    .expect("created session has an activity timestamp")
            };
            if original_last <= retention_start {
                continue;
            }
            let expected = source.revision;
            source.revision += 1;
            source.handed_to_session_id = Some(session_id.clone());
            tx.runs().update_agent_delivery(&source, expected)?;
            let delivery_sequence = tx.runs().agent_delivery_head()? + 1;
            tx.runs().insert_agent_delivery(&StoredAgentDelivery {
                sequence: delivery_sequence,
                message_id: message.id.clone(),
                project_id: project.clone(),
                recipient_assignment_id: None,
                recipient_human: false,
                state: "Pending".into(),
                revision: 1,
                delivered_at_ms: None,
                read_at_ms: None,
                recipient_session_id: Some(session_id.clone()),
                handed_over: true,
                handed_to_session_id: None,
                resolved_at_ms: None,
            })?;
            let event_sequence = tx.runs().agent_event_head()? + 1;
            tx.runs()
                .insert_agent_message_event(&StoredAgentMessageEvent {
                    sequence: event_sequence,
                    project_id: project.clone(),
                    message_id: message.id,
                    kind: "HandedOver".into(),
                    actor_id: session.agent_id.clone(),
                    recipient_assignment_id: None,
                    recipient_session_id: Some(session_id.clone()),
                    recipient_human: false,
                    created_at_ms: now,
                })?;
            inherited += 1;
        }
        if page_len < 500 {
            break;
        }
    }
    Ok(inherited)
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
    let mut item_versions = Vec::new();
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
        if let Some(resolution) = resolution {
            let blocking_item = message
                .blocking
                .then(|| message.target_item_id.clone())
                .flatten();
            let before = blocking_item
                .as_ref()
                .map(|item| tx.runs().open_blocking_question(item))
                .transpose()?;
            if let Some(updated) =
                resolve_message(tx, ctx, project, recipient, &message, resolution)?
            {
                if let (Some(item), Some(before)) = (blocking_item.as_ref(), before)
                    && let Some(version) = awaiting_human_change(tx, ctx, item, before)?
                {
                    item_versions.push(version);
                }
                if let Some(entry) = changed.iter_mut().find(|(_, row)| row.id == message.id) {
                    *entry = updated;
                } else {
                    changed.push(updated);
                }
            }
        }
    }
    Ok(Acknowledged {
        changed,
        item_versions,
    })
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
            handed_to_session_id: None,
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
