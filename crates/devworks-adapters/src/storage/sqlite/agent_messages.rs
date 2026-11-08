//! SQLite rows for durable agent messages, recipient deliveries and project stream events.
use super::conn::db;
use devworks_core::ports::storage::{
    MAX_PAGE, StorageError, StoredAgentAssignmentContext, StoredAgentDelivery, StoredAgentMessage,
    StoredAgentMessageEvent,
};
use devworks_core::protocol::{ActorId, AssignmentId, Id, ProjectId, RootId};
use rusqlite::{Connection, OptionalExtension, Row, params};

const MESSAGE_COLUMNS: &str = "id,project_id,root_id,channel,from_assignment_id,to_assignment_id,message_class,body,attachments_json,in_reply_to_id,state,blocking,idempotency_key,revision,created_at,resolved_at,from_actor_id,target_item_id,target_feature_id,resource_name,target_session_id,to_human,sender_session_id,resolved_by_actor_id,resolved_by_session_id";
const DELIVERY_COLUMNS: &str = "sequence,message_id,project_id,recipient_assignment_id,recipient_human,state,revision,delivered_at,read_at,recipient_session_id,handed_over,resolved_at";
const EVENT_COLUMNS: &str = "sequence,project_id,message_id,kind,actor_id,recipient_assignment_id,created_at,recipient_session_id,recipient_human";

pub(super) fn agent_assignment_context(
    conn: &Connection,
    assignment: &AssignmentId,
) -> Result<Option<StoredAgentAssignmentContext>, StorageError> {
    let mut stmt = db(conn.prepare("SELECT assignment_id,project_id,role_id,role_instructions,role_revisions_json,brief,task_ids_json,source_checkpoint_id,created_at FROM agent_assignment_context WHERE assignment_id=?1"))?;
    let mut rows = db(stmt.query([assignment.0.as_str()]))?;
    let Some(row) = db(rows.next())? else {
        return Ok(None);
    };
    let revisions: String = db(row.get(4))?;
    let tasks: String = db(row.get(6))?;
    Ok(Some(StoredAgentAssignmentContext {
        assignment_id: AssignmentId(id(db(row.get(0))?)?),
        project_id: ProjectId(id(db(row.get(1))?)?),
        role_id: id(db(row.get(2))?)?,
        role_instructions: db(row.get(3))?,
        role_revisions: serde_json::from_str(&revisions).map_err(|_| corrupt("role revisions"))?,
        brief: db(row.get(5))?,
        task_ids: serde_json::from_str(&tasks).map_err(|_| corrupt("task references"))?,
        source_checkpoint_id: optional_id(db(row.get(7))?)?,
        created_at_ms: db(row.get(8))?,
    }))
}

pub(super) fn insert_agent_assignment_context(
    conn: &Connection,
    context: &StoredAgentAssignmentContext,
) -> Result<(), StorageError> {
    if assignment_project(conn, &context.assignment_id)?.as_deref()
        != Some(context.project_id.0.as_str())
    {
        return Err(StorageError::ScopeDenied);
    }
    db(conn.execute(
        "INSERT INTO agent_assignment_context(assignment_id,project_id,role_id,role_instructions,role_revisions_json,brief,task_ids_json,source_checkpoint_id,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![context.assignment_id.0.as_str(),context.project_id.0.as_str(),context.role_id.as_str(),context.role_instructions,context.role_revisions.to_string(),context.brief,context.task_ids.to_string(),context.source_checkpoint_id.as_ref().map(Id::as_str),context.created_at_ms],
    ))?;
    Ok(())
}

fn corrupt(what: &str) -> StorageError {
    StorageError::InvalidRecord(format!("invalid stored agent {what}"))
}
fn id(value: String) -> Result<Id, StorageError> {
    Id::new(value).map_err(|_| corrupt("identifier"))
}
fn optional_id(value: Option<String>) -> Result<Option<Id>, StorageError> {
    value.map(id).transpose()
}
fn page(limit: u16) -> Result<i64, StorageError> {
    if limit == 0 || limit > MAX_PAGE {
        return Err(StorageError::InvalidRecord(
            "agent page limit out of range".into(),
        ));
    }
    Ok(i64::from(limit))
}
fn assignment_project(
    conn: &Connection,
    assignment: &AssignmentId,
) -> Result<Option<String>, StorageError> {
    db(conn.query_row(
        "SELECT w.project_id FROM assignments a JOIN work_items w ON w.id=a.root_id WHERE a.id=?1",
        [assignment.0.as_str()], |r| r.get(0)
    ).optional())
}
fn work_project(conn: &Connection, work: &RootId) -> Result<Option<String>, StorageError> {
    db(conn
        .query_row(
            "SELECT project_id FROM work_items WHERE id=?1",
            [work.0.as_str()],
            |r| r.get(0),
        )
        .optional())
}
fn session_project(conn: &Connection, session: &Id) -> Result<Option<String>, StorageError> {
    db(conn
        .query_row(
            "SELECT w.project_id FROM sessions s JOIN work_items w ON w.id=s.root_id WHERE s.id=?1",
            [session.as_str()],
            |r| r.get(0),
        )
        .optional())
}
fn message_row(row: &Row<'_>) -> Result<StoredAgentMessage, StorageError> {
    let attachments: String = db(row.get(8))?;
    Ok(StoredAgentMessage {
        id: id(db(row.get(0))?)?,
        project_id: ProjectId(id(db(row.get(1))?)?),
        root_id: optional_id(db(row.get(2))?)?.map(RootId),
        channel: db(row.get(3))?,
        from_assignment_id: optional_id(db(row.get(4))?)?.map(AssignmentId),
        to_assignment_id: optional_id(db(row.get(5))?)?.map(AssignmentId),
        message_class: db(row.get(6))?,
        body: db(row.get(7))?,
        attachments: serde_json::from_str(&attachments).map_err(|_| corrupt("attachments"))?,
        in_reply_to_id: optional_id(db(row.get(9))?)?,
        state: db(row.get(10))?,
        blocking: db(row.get(11))?,
        idempotency_key: db(row.get(12))?,
        revision: db(row.get(13))?,
        created_at_ms: db(row.get(14))?,
        resolved_at_ms: db(row.get(15))?,
        from_actor_id: ActorId(id(db(row.get(16))?)?),
        target_item_id: optional_id(db(row.get(17))?)?.map(RootId),
        target_feature_id: optional_id(db(row.get(18))?)?.map(RootId),
        resource_name: db(row.get(19))?,
        target_session_id: optional_id(db(row.get(20))?)?,
        to_human: db(row.get(21))?,
        sender_session_id: optional_id(db(row.get(22))?)?,
        resolved_by_actor_id: optional_id(db(row.get(23))?)?.map(ActorId),
        resolved_by_session_id: optional_id(db(row.get(24))?)?,
    })
}
fn delivery_row(row: &Row<'_>) -> Result<StoredAgentDelivery, StorageError> {
    Ok(StoredAgentDelivery {
        sequence: db(row.get(0))?,
        message_id: id(db(row.get(1))?)?,
        project_id: ProjectId(id(db(row.get(2))?)?),
        recipient_assignment_id: optional_id(db(row.get(3))?)?.map(AssignmentId),
        recipient_human: db(row.get(4))?,
        state: db(row.get(5))?,
        revision: db(row.get(6))?,
        delivered_at_ms: db(row.get(7))?,
        read_at_ms: db(row.get(8))?,
        recipient_session_id: optional_id(db(row.get(9))?)?,
        handed_over: db(row.get(10))?,
        resolved_at_ms: db(row.get(11))?,
    })
}
fn event_row(row: &Row<'_>) -> Result<StoredAgentMessageEvent, StorageError> {
    Ok(StoredAgentMessageEvent {
        sequence: db(row.get(0))?,
        project_id: ProjectId(id(db(row.get(1))?)?),
        message_id: id(db(row.get(2))?)?,
        kind: db(row.get(3))?,
        actor_id: ActorId(id(db(row.get(4))?)?),
        recipient_assignment_id: optional_id(db(row.get(5))?)?.map(AssignmentId),
        created_at_ms: db(row.get(6))?,
        recipient_session_id: optional_id(db(row.get(7))?)?,
        recipient_human: db(row.get(8))?,
    })
}

pub(super) fn message(
    conn: &Connection,
    key: &Id,
) -> Result<Option<StoredAgentMessage>, StorageError> {
    let mut stmt = db(conn.prepare(&format!(
        "SELECT {MESSAGE_COLUMNS} FROM agent_messages WHERE id=?1"
    )))?;
    let mut rows = db(stmt.query([key.as_str()]))?;
    match db(rows.next())? {
        Some(row) => message_row(row).map(Some),
        None => Ok(None),
    }
}
pub(super) fn by_key(
    conn: &Connection,
    project: &ProjectId,
    sender: &ActorId,
    key: &str,
) -> Result<Option<StoredAgentMessage>, StorageError> {
    let mut stmt = db(conn.prepare(&format!("SELECT {MESSAGE_COLUMNS} FROM agent_messages WHERE project_id=?1 AND from_actor_id=?2 AND idempotency_key=?3")))?;
    let mut rows = db(stmt.query(params![project.0.as_str(), sender.0.as_str(), key]))?;
    match db(rows.next())? {
        Some(row) => message_row(row).map(Some),
        None => Ok(None),
    }
}
pub(super) fn messages(
    conn: &Connection,
    project: &ProjectId,
    before: Option<&Id>,
    limit: u16,
) -> Result<Vec<StoredAgentMessage>, StorageError> {
    let (time, key) = match before {
        Some(key) => {
            let row = message(conn, key)?.ok_or_else(|| StorageError::NotFound {
                kind: "agent_message",
                id: key.as_str().into(),
            })?;
            if &row.project_id != project {
                return Err(StorageError::ScopeDenied);
            }
            (row.created_at_ms, key.as_str())
        }
        None => (i64::MAX, "~"),
    };
    let mut stmt = db(conn.prepare(&format!("SELECT {MESSAGE_COLUMNS} FROM agent_messages WHERE project_id=?1 AND (created_at<?2 OR (created_at=?2 AND id<?3)) ORDER BY created_at DESC,id DESC LIMIT ?4")))?;
    let mut rows = db(stmt.query(params![project.0.as_str(), time, key, page(limit)?]))?;
    let mut out = Vec::new();
    while let Some(row) = db(rows.next())? {
        out.push(message_row(row)?);
    }
    Ok(out)
}
pub(super) fn assignment_thread(
    conn: &Connection,
    project: &ProjectId,
    assignment: &AssignmentId,
    direction: &str,
    unacknowledged_only: bool,
    before: Option<&Id>,
    limit: u16,
) -> Result<Vec<(StoredAgentMessage, Option<StoredAgentDelivery>)>, StorageError> {
    if !matches!(direction, "Inbound" | "Outbound" | "Both") {
        return Err(StorageError::InvalidRecord(
            "invalid message direction".into(),
        ));
    }
    if assignment_project(conn, assignment)?.as_deref() != Some(project.0.as_str()) {
        return Err(StorageError::ScopeDenied);
    }
    let (time, key) = match before {
        Some(key) => {
            let m = message(conn, key)?.ok_or(StorageError::CursorExpired)?;
            if m.project_id != *project {
                return Err(StorageError::CursorExpired);
            }
            (m.created_at_ms, key.as_str())
        }
        None => (i64::MAX, "~"),
    };
    let mut stmt=db(conn.prepare(
        "SELECT m.id FROM agent_messages m LEFT JOIN agent_message_deliveries d ON d.message_id=m.id AND d.recipient_assignment_id=?2 AND d.recipient_session_id IS NULL WHERE m.project_id=?1 AND ((?3='Inbound' AND d.sequence IS NOT NULL) OR (?3='Outbound' AND m.from_assignment_id=?2) OR (?3='Both' AND (d.sequence IS NOT NULL OR m.from_assignment_id=?2))) AND (NOT ?4 OR d.state IN ('Pending','Delivered')) AND (m.created_at<?5 OR (m.created_at=?5 AND m.id<?6)) ORDER BY m.created_at DESC,m.id DESC LIMIT ?7"
    ))?;
    let mut rows = db(stmt.query(params![
        project.0.as_str(),
        assignment.0.as_str(),
        direction,
        unacknowledged_only,
        time,
        key,
        page(limit)?
    ]))?;
    let mut ids = Vec::new();
    while let Some(row) = db(rows.next())? {
        ids.push(id(db(row.get(0))?)?);
    }
    drop(rows);
    drop(stmt);
    ids.into_iter()
        .map(|key| {
            let m = message(conn, &key)?.ok_or_else(|| corrupt("thread message"))?;
            let d = delivery(conn, &key, Some(assignment))?;
            Ok((m, d))
        })
        .collect()
}
pub(super) fn delivery(
    conn: &Connection,
    message: &Id,
    recipient: Option<&AssignmentId>,
) -> Result<Option<StoredAgentDelivery>, StorageError> {
    let mut stmt = db(conn.prepare(&format!("SELECT {DELIVERY_COLUMNS} FROM agent_message_deliveries WHERE message_id=?1 AND recipient_assignment_id IS ?2 AND recipient_session_id IS NULL")))?;
    let mut rows = db(stmt.query(params![message.as_str(), recipient.map(|a| a.0.as_str())]))?;
    match db(rows.next())? {
        Some(row) => delivery_row(row).map(Some),
        None => Ok(None),
    }
}
pub(super) fn session_delivery(
    conn: &Connection,
    message: &Id,
    session: &Id,
) -> Result<Option<StoredAgentDelivery>, StorageError> {
    let mut stmt = db(conn.prepare(&format!("SELECT {DELIVERY_COLUMNS} FROM agent_message_deliveries WHERE message_id=?1 AND recipient_session_id=?2")))?;
    let mut rows = db(stmt.query(params![message.as_str(), session.as_str()]))?;
    match db(rows.next())? {
        Some(row) => delivery_row(row).map(Some),
        None => Ok(None),
    }
}
pub(super) fn inbox(
    conn: &Connection,
    project: &ProjectId,
    recipient: Option<&AssignmentId>,
    after: i64,
    include_read: bool,
    limit: u16,
) -> Result<Vec<(StoredAgentDelivery, StoredAgentMessage)>, StorageError> {
    if after < 0 {
        return Err(StorageError::InvalidRecord(
            "negative delivery cursor".into(),
        ));
    }
    let mut stmt = db(conn.prepare(&format!("SELECT {DELIVERY_COLUMNS} FROM agent_message_deliveries WHERE project_id=?1 AND recipient_assignment_id IS ?2 AND recipient_session_id IS NULL AND sequence>?3 AND (?4 OR state IN ('Pending','Delivered')) ORDER BY sequence LIMIT ?5")))?;
    let mut rows = db(stmt.query(params![
        project.0.as_str(),
        recipient.map(|a| a.0.as_str()),
        after,
        include_read,
        page(limit)?
    ]))?;
    let mut deliveries = Vec::new();
    while let Some(row) = db(rows.next())? {
        deliveries.push(delivery_row(row)?);
    }
    drop(rows);
    drop(stmt);
    deliveries
        .into_iter()
        .map(|delivery| {
            let message =
                message(conn, &delivery.message_id)?.ok_or_else(|| corrupt("delivery message"))?;
            Ok((delivery, message))
        })
        .collect()
}
pub(super) fn session_inbox(
    conn: &Connection,
    project: &ProjectId,
    session: &Id,
    after: i64,
    include_read: bool,
    limit: u16,
) -> Result<Vec<(StoredAgentDelivery, StoredAgentMessage)>, StorageError> {
    if after < 0 {
        return Err(StorageError::InvalidRecord(
            "negative delivery cursor".into(),
        ));
    }
    let mut stmt = db(conn.prepare(&format!("SELECT {DELIVERY_COLUMNS} FROM agent_message_deliveries WHERE project_id=?1 AND recipient_session_id=?2 AND sequence>?3 AND (?4 OR state IN ('Pending','Delivered')) ORDER BY sequence LIMIT ?5")))?;
    let mut rows = db(stmt.query(params![
        project.0.as_str(),
        session.as_str(),
        after,
        include_read,
        page(limit)?
    ]))?;
    let mut deliveries = Vec::new();
    while let Some(row) = db(rows.next())? {
        deliveries.push(delivery_row(row)?);
    }
    drop(rows);
    drop(stmt);
    deliveries
        .into_iter()
        .map(|delivery| {
            let message =
                message(conn, &delivery.message_id)?.ok_or_else(|| corrupt("delivery message"))?;
            Ok((delivery, message))
        })
        .collect()
}
pub(super) fn delivery_head(conn: &Connection) -> Result<i64, StorageError> {
    db(conn.query_row(
        "SELECT coalesce(max(sequence),0) FROM agent_message_deliveries",
        [],
        |r| r.get(0),
    ))
}
pub(super) fn events(
    conn: &Connection,
    project: &ProjectId,
    after: i64,
    limit: u16,
) -> Result<Vec<StoredAgentMessageEvent>, StorageError> {
    if after < 0 {
        return Err(StorageError::InvalidRecord("negative event cursor".into()));
    }
    let mut stmt = db(conn.prepare(&format!("SELECT {EVENT_COLUMNS} FROM agent_message_events WHERE project_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3")))?;
    let mut rows = db(stmt.query(params![project.0.as_str(), after, page(limit)?]))?;
    let mut out = Vec::new();
    while let Some(row) = db(rows.next())? {
        out.push(event_row(row)?);
    }
    Ok(out)
}
pub(super) fn event_head(conn: &Connection) -> Result<i64, StorageError> {
    db(conn.query_row(
        "SELECT coalesce(max(sequence),0) FROM agent_message_events",
        [],
        |r| r.get(0),
    ))
}

pub(super) fn insert_message(
    conn: &Connection,
    m: &StoredAgentMessage,
) -> Result<(), StorageError> {
    if m.revision != 1
        || m.body.is_empty()
        || m.body.chars().count() > 65536
        || m.body.contains('\0')
        || m.idempotency_key
            .as_ref()
            .is_some_and(|s| s.len() > 256 || s.is_empty())
        || !m.attachments.is_array()
        || m.resolved_at_ms.is_some()
        || m.resolved_by_actor_id.is_some()
        || m.resolved_by_session_id.is_some()
        || m.resource_name
            .as_ref()
            .is_some_and(|r| r.is_empty() || r.len() > 200 || r.chars().any(char::is_control))
    {
        return Err(StorageError::InvalidRecord(
            "invalid agent message body, key or revision".into(),
        ));
    }
    if m.channel == "Assignment" && m.to_assignment_id.is_none()
        || !matches!(
            m.channel.as_str(),
            "Assignment"
                | "ProjectBoard"
                | "Item"
                | "Project"
                | "Feature"
                | "Resource"
                | "Session"
                | "Human"
        )
        || m.channel == "Item" && m.target_item_id.is_none()
        || m.channel == "Feature" && m.target_feature_id.is_none()
        || m.channel == "Resource" && m.resource_name.is_none()
        || m.channel == "Session" && m.target_session_id.is_none()
        || m.channel == "Human" && !m.to_human
        || m.blocking
            && (m.message_class != "Question" || !m.to_human || m.target_item_id.is_none())
    {
        return Err(StorageError::InvalidRecord(
            "invalid message channel".into(),
        ));
    }
    for work in [
        m.root_id.as_ref(),
        m.target_item_id.as_ref(),
        m.target_feature_id.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if work_project(conn, work)?.as_deref() != Some(m.project_id.0.as_str()) {
            return Err(StorageError::ScopeDenied);
        }
    }
    for session in [m.target_session_id.as_ref(), m.sender_session_id.as_ref()]
        .into_iter()
        .flatten()
    {
        if session_project(conn, session)?.as_deref() != Some(m.project_id.0.as_str()) {
            return Err(StorageError::ScopeDenied);
        }
    }
    for assignment in [m.from_assignment_id.as_ref(), m.to_assignment_id.as_ref()]
        .into_iter()
        .flatten()
    {
        let project = assignment_project(conn, assignment)?;
        if project.as_deref() != Some(m.project_id.0.as_str()) {
            return Err(StorageError::ScopeDenied);
        }
    }
    if let Some(parent) = &m.in_reply_to_id {
        if message(conn, parent)?.as_ref().map(|p| &p.project_id) != Some(&m.project_id) {
            return Err(StorageError::ScopeDenied);
        }
    }
    db(conn.execute(
        "INSERT INTO agent_messages(id,project_id,root_id,channel,from_assignment_id,to_assignment_id,message_class,body,attachments_json,in_reply_to_id,state,blocking,idempotency_key,revision,created_at,resolved_at,from_actor_id,target_item_id,target_feature_id,resource_name,target_session_id,to_human,sender_session_id,resolved_by_actor_id,resolved_by_session_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25)",
        params![m.id.as_str(),m.project_id.0.as_str(),m.root_id.as_ref().map(|r|r.0.as_str()),m.channel,m.from_assignment_id.as_ref().map(|a|a.0.as_str()),m.to_assignment_id.as_ref().map(|a|a.0.as_str()),m.message_class,m.body,m.attachments.to_string(),m.in_reply_to_id.as_ref().map(Id::as_str),m.state,m.blocking,m.idempotency_key,m.revision,m.created_at_ms,m.resolved_at_ms,m.from_actor_id.0.as_str(),m.target_item_id.as_ref().map(|r|r.0.as_str()),m.target_feature_id.as_ref().map(|r|r.0.as_str()),m.resource_name,m.target_session_id.as_ref().map(Id::as_str),m.to_human,m.sender_session_id.as_ref().map(Id::as_str),m.resolved_by_actor_id.as_ref().map(|a|a.0.as_str()),m.resolved_by_session_id.as_ref().map(Id::as_str)]
    ))?;
    Ok(())
}
pub(super) fn resolve_message(
    conn: &Connection,
    m: &StoredAgentMessage,
    expected: i64,
) -> Result<(), StorageError> {
    let current = message(conn, &m.id)?.ok_or_else(|| StorageError::NotFound {
        kind: "agent_message",
        id: m.id.as_str().into(),
    })?;
    if current.revision != expected {
        return Err(StorageError::InvalidRecord(
            "stale agent message revision".into(),
        ));
    }
    if current.state.as_deref() != Some("Open")
        || !matches!(m.state.as_deref(), Some("Answered" | "Declined"))
        || m.revision != expected + 1
        || m.resolved_at_ms.is_none()
        || m.created_at_ms != current.created_at_ms
        || m.project_id != current.project_id
        || m.from_assignment_id != current.from_assignment_id
        || m.to_assignment_id != current.to_assignment_id
        || m.body != current.body
        || m.attachments != current.attachments
        || m.message_class != current.message_class
        || m.blocking != current.blocking
        || m.in_reply_to_id != current.in_reply_to_id
        || m.idempotency_key != current.idempotency_key
        || m.channel != current.channel
        || m.root_id != current.root_id
        || m.from_actor_id != current.from_actor_id
        || m.target_item_id != current.target_item_id
        || m.target_feature_id != current.target_feature_id
        || m.resource_name != current.resource_name
        || m.target_session_id != current.target_session_id
        || m.to_human != current.to_human
        || m.sender_session_id != current.sender_session_id
        || m.resolved_by_actor_id.is_none()
    {
        return Err(StorageError::InvalidRecord(
            "illegal agent message resolution".into(),
        ));
    }
    db(conn.execute(
        "UPDATE agent_messages SET state=?2,revision=?3,resolved_at=?4,resolved_by_actor_id=?6,resolved_by_session_id=?7 WHERE id=?1 AND revision=?5",
        params![
            m.id.as_str(),
            m.state,
            m.revision,
            m.resolved_at_ms,
            expected
            ,m.resolved_by_actor_id.as_ref().map(|a|a.0.as_str()),m.resolved_by_session_id.as_ref().map(Id::as_str)
        ],
    ))?;
    Ok(())
}
pub(super) fn insert_delivery(
    conn: &Connection,
    d: &StoredAgentDelivery,
) -> Result<(), StorageError> {
    if d.revision != 1
        || d.sequence <= 0
        || d.state != "Pending"
        || d.delivered_at_ms.is_some()
        || d.read_at_ms.is_some()
        || d.resolved_at_ms.is_some()
        || i32::from(d.recipient_assignment_id.is_some())
            + i32::from(d.recipient_session_id.is_some())
            + i32::from(d.recipient_human)
            != 1
    {
        return Err(StorageError::InvalidRecord(
            "invalid initial agent delivery".into(),
        ));
    }
    let m = message(conn, &d.message_id)?.ok_or_else(|| StorageError::NotFound {
        kind: "agent_message",
        id: d.message_id.as_str().into(),
    })?;
    if m.project_id != d.project_id
        || (m.channel == "Assignment" && m.to_assignment_id != d.recipient_assignment_id)
        || (m.channel == "Session" && m.target_session_id != d.recipient_session_id)
    {
        return Err(StorageError::ScopeDenied);
    }
    if let Some(recipient) = &d.recipient_assignment_id
        && assignment_project(conn, recipient)?.as_deref() != Some(d.project_id.0.as_str())
    {
        return Err(StorageError::ScopeDenied);
    }
    if let Some(session) = &d.recipient_session_id
        && session_project(conn, session)?.as_deref() != Some(d.project_id.0.as_str())
    {
        return Err(StorageError::ScopeDenied);
    }
    db(conn.execute("INSERT INTO agent_message_deliveries(sequence,message_id,project_id,recipient_assignment_id,recipient_human,state,revision,delivered_at,read_at,recipient_session_id,handed_over,resolved_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",params![d.sequence,d.message_id.as_str(),d.project_id.0.as_str(),d.recipient_assignment_id.as_ref().map(|a|a.0.as_str()),d.recipient_human,d.state,d.revision,d.delivered_at_ms,d.read_at_ms,d.recipient_session_id.as_ref().map(Id::as_str),d.handed_over,d.resolved_at_ms]))?;
    Ok(())
}
pub(super) fn update_delivery(
    conn: &Connection,
    d: &StoredAgentDelivery,
    expected: i64,
) -> Result<(), StorageError> {
    let current = (if let Some(session) = &d.recipient_session_id {
        session_delivery(conn, &d.message_id, session)?
    } else {
        delivery(conn, &d.message_id, d.recipient_assignment_id.as_ref())?
    })
    .ok_or_else(|| StorageError::NotFound {
        kind: "agent_delivery",
        id: d.message_id.as_str().into(),
    })?;
    if current.revision != expected
        || d.revision != expected + 1
        || d.sequence != current.sequence
        || d.project_id != current.project_id
        || d.message_id != current.message_id
        || d.recipient_assignment_id != current.recipient_assignment_id
        || d.recipient_session_id != current.recipient_session_id
        || d.recipient_human != current.recipient_human
        || (current.handed_over && !d.handed_over)
    {
        return Err(StorageError::InvalidRecord(
            "stale or mutated agent delivery".into(),
        ));
    }
    let legal = matches!(
        (current.state.as_str(), d.state.as_str()),
        ("Pending", "Delivered")
            | ("Pending", "Read")
            | ("Delivered", "Read")
            | ("Pending", "Answered")
            | ("Pending", "Declined")
            | ("Delivered", "Answered")
            | ("Delivered", "Declined")
            | ("Read", "Answered")
            | ("Read", "Declined")
    );
    if !legal
        || (d.state != "Pending" && d.delivered_at_ms.is_none())
        || (matches!(d.state.as_str(), "Read" | "Answered" | "Declined") && d.read_at_ms.is_none())
        || (matches!(d.state.as_str(), "Answered" | "Declined") != d.resolved_at_ms.is_some())
    {
        return Err(StorageError::InvalidRecord(
            "illegal agent delivery transition".into(),
        ));
    }
    db(conn.execute("UPDATE agent_message_deliveries SET state=?2,revision=?3,delivered_at=?4,read_at=?5,resolved_at=?7,handed_over=?8 WHERE sequence=?1 AND revision=?6",params![d.sequence,d.state,d.revision,d.delivered_at_ms,d.read_at_ms,expected,d.resolved_at_ms,d.handed_over]))?;
    Ok(())
}
pub(super) fn insert_event(
    conn: &Connection,
    e: &StoredAgentMessageEvent,
) -> Result<(), StorageError> {
    if e.sequence <= 0 {
        return Err(StorageError::InvalidRecord(
            "invalid agent event sequence".into(),
        ));
    }
    let m = message(conn, &e.message_id)?.ok_or_else(|| StorageError::NotFound {
        kind: "agent_message",
        id: e.message_id.as_str().into(),
    })?;
    if m.project_id != e.project_id
        || (m.channel == "Assignment"
            && e.recipient_assignment_id.is_some()
            && m.to_assignment_id != e.recipient_assignment_id)
    {
        return Err(StorageError::ScopeDenied);
    }
    if let Some(recipient) = &e.recipient_assignment_id
        && assignment_project(conn, recipient)?.as_deref() != Some(e.project_id.0.as_str())
    {
        return Err(StorageError::ScopeDenied);
    }
    if let Some(session) = &e.recipient_session_id
        && session_project(conn, session)?.as_deref() != Some(e.project_id.0.as_str())
    {
        return Err(StorageError::ScopeDenied);
    }
    db(conn.execute("INSERT INTO agent_message_events(sequence,project_id,message_id,kind,actor_id,recipient_assignment_id,created_at,recipient_session_id,recipient_human) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![e.sequence,e.project_id.0.as_str(),e.message_id.as_str(),e.kind,e.actor_id.0.as_str(),e.recipient_assignment_id.as_ref().map(|a|a.0.as_str()),e.created_at_ms,e.recipient_session_id.as_ref().map(Id::as_str),e.recipient_human]))?;
    Ok(())
}
