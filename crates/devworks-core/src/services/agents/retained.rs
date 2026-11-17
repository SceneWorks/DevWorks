//! The retained work-message inbox's single transactional delivery pass. A gateway command can
//! long-poll by repeating this pass between transactions; no connection is held during a wait.
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::storage::{
    ANY_QUEUE, AgentMessageHistoryFilter, ReadSnapshot, StorageError, StoredAgentDelivery,
    StoredAgentMessage, StoredAgentMessageEvent, Unavailable, WriteTransaction,
};
use crate::protocol::{
    AssignmentId, DomainErrorCode, Id, ProjectId, RetryAdvice, Revision, RevisionDimension, RootId,
};
use crate::services::agents::rows;
use crate::services::cursor::CursorBinding;
use crate::services::failure::{Failure, Refusal};
use std::collections::HashSet;

/// A rule the caller's authority or scope does not satisfy, naming what it concerns.
fn denied(kind: &'static str) -> Failure {
    Refusal::new(DomainErrorCode::PermissionDenied, kind, None).into()
}
/// A malformed request argument, naming the argument.
fn invalid(kind: &'static str) -> Failure {
    Refusal::new(DomainErrorCode::InvalidArgument, kind, None).into()
}
/// A cursor this caller cannot continue from; a refreshed read gives a new one.
fn expired(kind: &'static str) -> Failure {
    Refusal::new(DomainErrorCode::CursorExpired, kind, None)
        .retry(RetryAdvice::RefreshAndNewRequest)
        .into()
}

mod post;
pub use post::{MessageKind, PostSpec, PostedMessage, post_host_notice, post_message};

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

/// What an inbox cursor is bound to: the caller, its project and recipient, the store and
/// restore epoch. A delivery sequence is store-wide and only grows, so the cursor stays valid
/// across commits; no scope revision is bound.
pub fn inbox_binding(
    ctx: &InvocationContext,
    project: &ProjectId,
    recipient: &Recipient,
) -> CursorBinding {
    let recipient = match recipient {
        Recipient::Session(id) => id.as_str(),
        Recipient::Human => "",
    };
    CursorBinding::new(ctx, "agents.receive", &[project.0.as_str(), recipient], 0)
}

/// The inbox cursor at delivery `sequence`. It is an observation position, never authority;
/// every read and acknowledgment rechecks the current recipient and project before using it.
pub fn inbox_cursor(binding: &CursorBinding, sequence: i64) -> Result<String, Failure> {
    if sequence < 0 {
        return Err(invalid("cursor"));
    }
    Ok(binding.encode(&sequence.to_string()))
}

/// The delivery sequence of an inbox cursor; any other cursor is `CursorExpired`.
pub fn parse_inbox_cursor(binding: &CursorBinding, cursor: &str) -> Result<i64, Failure> {
    let position = match binding.decode(cursor) {
        Ok(position) => position,
        Err(StorageError::CursorExpired) => return Err(expired("agent_inbox_cursor")),
        Err(error) => return Err(error.into()),
    };
    position
        .parse::<i64>()
        .ok()
        .filter(|sequence| *sequence >= 0)
        .ok_or_else(|| expired("agent_inbox_cursor"))
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
) -> Result<MessageEventPage, Failure> {
    if limit == 0 || limit > 100 {
        return Err(invalid("limit"));
    }
    if after.is_some_and(|cursor| cursor < 0) {
        return Err(invalid("cursor"));
    }
    if ctx.principal_kind() != PrincipalKind::Human
        || ctx.session_id().is_some()
        || snapshot.actors().actor_kind(ctx.principal())? != Some(PrincipalKind::Human)
    {
        return Err(denied("principal"));
    }
    if ctx.project_id().is_some_and(|scope| scope != project)
        || !snapshot.sessions().project_exists(project)?
    {
        return Err(denied("project"));
    }
    let head = snapshot.runs().agent_event_head()?;
    let position = after.unwrap_or(head);
    if position > head {
        return Err(expired("agent_event_cursor"));
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
) -> Result<MessageHistoryPage, Failure> {
    if query.limit == 0 || query.limit > 200 {
        return Err(invalid("limit"));
    }
    if query.before.is_some_and(|n| n < 0) {
        return Err(invalid("before"));
    }
    if query.channel.as_deref().is_some_and(|channel| {
        !matches!(
            channel,
            "Item" | "Project" | "Feature" | "Resource" | "Session" | "Human"
        )
    }) {
        return Err(invalid("channel"));
    }
    if query.resource.as_deref().is_some_and(str::is_empty) {
        return Err(invalid("resource"));
    }
    if ctx.principal_kind() != PrincipalKind::Human
        || ctx.session_id().is_some()
        || snapshot.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
    {
        return Err(denied("principal"));
    }
    if ctx
        .project_id()
        .is_some_and(|scope| scope != &query.project)
        || !snapshot.sessions().project_exists(&query.project)?
    {
        return Err(denied("project"));
    }
    for (target, feature, kind) in [
        (query.item.as_ref(), false, "item"),
        (query.feature.as_ref(), true, "feature"),
    ] {
        if let Some(target) = target {
            let work = snapshot
                .sessions()
                .work_root(target)?
                .ok_or_else(|| denied(kind))?;
            if work.project_id != query.project || (work.kind == "feature") != feature {
                return Err(denied(kind));
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
) -> Result<Option<ItemVersion>, Failure> {
    let after = tx.runs().open_blocking_question(item)?;
    if before == after {
        return Ok(None);
    }
    let mut work = tx.work().work(&item.0)?.ok_or_else(|| denied("item"))?;
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
) -> Result<(), Failure> {
    if tx.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind()) {
        return Err(denied("principal"));
    }
    if ctx.project_id().is_some_and(|scope| scope != project) {
        return Err(denied("project"));
    }
    match recipient {
        Recipient::Session(id) => {
            if ctx.principal_kind() != PrincipalKind::Agent || ctx.session_id() != Some(id) {
                return Err(denied("session"));
            }
            let session = tx
                .sessions()
                .session(id)?
                .ok_or_else(|| denied("session"))?;
            let root_project = match &session.root_id {
                Some(root) => tx.sessions().work_root(root)?.map(|work| work.project_id),
                None => None,
            };
            if session.project_id.is_some()
                && root_project.is_some()
                && session.project_id != root_project
            {
                return Err(denied("session"));
            }
            let session_project = session.project_id.clone().or(root_project);
            if session.closed()
                || session_project.as_ref() != Some(project)
                || session.agent_id != *ctx.principal()
                || ctx
                    .root_id()
                    .is_some_and(|root| session.root_id.as_ref() != Some(root))
            {
                return Err(denied("session"));
            }
        }
        Recipient::Human => {
            if ctx.principal_kind() != PrincipalKind::Human || ctx.session_id().is_some() {
                return Err(denied("principal"));
            }
            if tx.projects().project(project)?.is_none() {
                return Err(denied("project"));
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
) -> Result<(StoredAgentDelivery, StoredAgentMessage), Failure> {
    let delivery = match recipient {
        Recipient::Session(id) => tx.runs().agent_session_delivery(message_id, id)?,
        Recipient::Human => tx.runs().agent_delivery(message_id, None)?,
    }
    .ok_or_else(|| denied("agent_message"))?;
    if delivery.project_id != *project
        || delivery.recipient_human != matches!(recipient, Recipient::Human)
        || delivery.recipient_session_id.as_ref()
            != match recipient {
                Recipient::Session(id) => Some(id),
                Recipient::Human => None,
            }
    {
        return Err(denied("agent_message"));
    }
    let message = tx
        .runs()
        .agent_message(message_id)?
        .ok_or_else(|| denied("agent_message"))?;
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
) -> Result<DeliveredInbox, Failure> {
    if limit == 0 || limit > 100 {
        return Err(invalid("limit"));
    }
    // A cursor ahead of the inbox head cannot be one this inbox issued.
    if after < 0 || after > tx.runs().agent_delivery_head()? {
        return Err(invalid("cursor"));
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
pub(super) fn delegated_item_ids(
    tx: &mut dyn WriteTransaction,
    assignment: &crate::ports::storage::StoredAssignment,
) -> Result<Vec<RootId>, Failure> {
    if assignment.purpose != crate::ports::storage::AssignmentPurpose::Child
        || assignment.work_id == assignment.root_id
    {
        return Ok(Vec::new());
    }
    let Some(context) = tx.runs().agent_assignment_context(&assignment.id)? else {
        return Ok(Vec::new());
    };
    let tasks: Vec<Id> = serde_json::from_value(context.task_ids).map_err(|_| {
        Failure::from(StorageError::unavailable(
            Unavailable::Corrupt,
            "stored delegated task scope",
        ))
    })?;
    if !tasks.contains(&assignment.work_id.0) {
        return Ok(Vec::new());
    }
    let root = tx
        .sessions()
        .work_root(&assignment.root_id)?
        .ok_or_else(|| denied("root"))?;
    let mut current = Vec::new();
    for task in tasks {
        let id = RootId(task);
        let Some(work) = tx.sessions().work_root(&id)? else {
            continue;
        };
        if work.project_id == root.project_id
            && work.kind != "feature"
            && (if root.kind == "feature" {
                work.parent_feature_id.as_ref() == Some(&root.id)
            } else {
                work.id == root.id
            })
        {
            current.push(id);
        }
    }
    Ok(current)
}

pub fn handover_item(
    tx: &mut dyn WriteTransaction,
    assignment_id: &AssignmentId,
) -> Result<usize, Failure> {
    let assignment = tx
        .runs()
        .assignment(assignment_id)?
        .ok_or_else(|| denied("assignment"))?;
    if !assignment.state.is_live() {
        return Err(denied("assignment"));
    }
    let items = if assignment.purpose == crate::ports::storage::AssignmentPurpose::Child {
        let items = delegated_item_ids(tx, &assignment)?;
        if items.is_empty() {
            return Ok(0);
        }
        items
    } else {
        vec![assignment.work_id.clone()]
    };
    let session_id = &assignment.session_id;
    let work = tx
        .sessions()
        .work_root(&assignment.work_id)?
        .ok_or_else(|| denied("item"))?;
    let session = tx
        .sessions()
        .session(session_id)?
        .ok_or_else(|| denied("session"))?;
    if session.closed() || post::session_project(tx, &session)?.as_ref() != Some(&work.project_id) {
        return Err(denied("session"));
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
            if !message
                .target_item_id
                .as_ref()
                .is_some_and(|item| items.contains(item))
                || message.target_session_id.is_some()
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
            let target = rows::Target::Session(session_id.clone());
            rows::insert_delivery(tx, &work.project_id, &message.id, &target, true)?;
            rows::append_event(
                tx,
                &work.project_id,
                &message.id,
                "HandedOver",
                &session.agent_id,
                &target,
                now,
            )?;
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
pub fn handover_direct(tx: &mut dyn WriteTransaction, session_id: &Id) -> Result<usize, Failure> {
    let session = tx
        .sessions()
        .session(session_id)?
        .ok_or_else(|| denied("session"))?;
    if session.closed() {
        return Err(denied("session"));
    }
    let project = post::session_project(tx, &session)?.ok_or_else(|| denied("session"))?;
    let repositories = tx.sessions().session_repository_scope(session_id)?;
    let primary_sessions: HashSet<Id> = match &session.root_id {
        Some(root) => crate::services::runs::root_assignments(tx.runs(), root, false)?
            .into_iter()
            .filter(|assignment| {
                assignment.purpose == crate::ports::storage::AssignmentPurpose::Primary
            })
            .map(|assignment| assignment.session_id)
            .collect(),
        None => HashSet::new(),
    };
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
                .ok_or_else(|| denied("session"))?;
            let original = tx
                .sessions()
                .session(original_id)?
                .ok_or_else(|| denied("session"))?;
            if post::session_project(tx, &holder)?.as_ref() != Some(&project)
                || post::session_project(tx, &original)?.as_ref() != Some(&project)
                || tx.sessions().session_repository_scope(original_id)? != repositories
            {
                continue;
            }
            // Repository equality is an audience constraint, not proof of succession. A
            // continuing conversation or a new primary coordinator of the same root is the
            // only lineage this store can verify; a delegated child has neither identity.
            if session.root_id.is_none()
                || session.root_id != original.root_id
                || !((session.conversation_id == original.conversation_id
                    && session.agent_id == original.agent_id)
                    || (primary_sessions.contains(session_id)
                        && primary_sessions.contains(original_id)))
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
            let target = rows::Target::Session(session_id.clone());
            rows::insert_delivery(tx, &project, &message.id, &target, true)?;
            rows::append_event(
                tx,
                &project,
                &message.id,
                "HandedOver",
                &session.agent_id,
                &target,
                now,
            )?;
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
) -> Result<Acknowledged, Failure> {
    if resolution.is_some() && !matches!(selection, AckSelection::Messages(_)) {
        return Err(invalid("resolution"));
    }
    authorize_recipient(tx, ctx, project, recipient)?;
    let ids = match selection {
        AckSelection::Messages(ids) => {
            if ids.is_empty() || ids.len() > 100 {
                return Err(invalid("message_ids"));
            }
            let mut seen = HashSet::new();
            ids.into_iter()
                .filter(|id| seen.insert(id.clone()))
                .collect::<Vec<_>>()
        }
        AckSelection::Through(up_to) => {
            // An `up_to` ahead of the inbox head cannot be a cursor this inbox issued.
            if up_to < 0 || up_to > tx.runs().agent_delivery_head()? {
                return Err(invalid("up_to"));
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
        if !rows::is_read(&delivery) {
            rows::mark_read(tx, &mut delivery, now)?;
            rows::append_event(
                tx,
                project,
                &id,
                "Read",
                ctx.principal(),
                &rows::Target::from(recipient),
                now,
            )?;
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
) -> Result<Option<(StoredAgentDelivery, StoredAgentMessage)>, Failure> {
    authorize_recipient(tx, ctx, project, recipient)?;
    let (mut delivery, _) = delivery_for(tx, project, recipient, &message.id)?;
    if message.to_human && !matches!(recipient, Recipient::Human) {
        return Err(denied("agent_message"));
    }
    if !matches!(message.message_class.as_str(), "Question" | "Request") {
        return Err(invalid("message_class"));
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
        )
        .into());
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
    let expected = delivery.revision;
    delivery.state = resolution.state().into();
    delivery.revision += 1;
    delivery.delivered_at_ms.get_or_insert(now);
    delivery.read_at_ms.get_or_insert(now);
    delivery.resolved_at_ms = Some(now);
    tx.runs().update_agent_delivery(&delivery, expected)?;
    rows::append_event(
        tx,
        project,
        &message.id,
        resolution.state(),
        ctx.principal(),
        &rows::Target::from(recipient),
        now,
    )?;
    Ok(Some((delivery, resolved)))
}
