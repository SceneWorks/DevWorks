//! The append-only event log and its capability-gated view.
use super::*;

pub(crate) const EVENT_COLUMNS: &str = "seq,id,actor_id,project_id,root_id,operation_id,aggregate_kind,\
    aggregate_id,aggregate_revision,kind,created_at,payload_json";

pub(crate) fn event_row(r: &Row<'_>) -> rusqlite::Result<[Option<String>; 12]> {
    let mut out: [Option<String>; 12] = Default::default();
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = match r.get_ref(i)? {
            rusqlite::types::ValueRef::Null => None,
            rusqlite::types::ValueRef::Integer(v) => Some(v.to_string()),
            v => Some(v.as_str()?.to_owned()),
        };
    }
    Ok(out)
}

pub(crate) fn event_of(raw: [Option<String>; 12]) -> Result<EventRecord, StorageError> {
    let [
        seq,
        id,
        actor,
        project,
        root,
        operation,
        akind,
        aid,
        arev,
        kind,
        created,
        payload,
    ] = raw;
    let int = |v: Option<String>| -> Result<i64, StorageError> {
        v.and_then(|s| s.parse().ok())
            .ok_or_else(|| corrupt("event integer"))
    };
    let txt = |v: Option<String>| v.ok_or_else(|| corrupt("event text"));
    Ok(EventRecord {
        sequence: int(seq)?,
        id: EventId(id_of(txt(id)?)?),
        actor: ActorId(id_of(txt(actor)?)?),
        project_id: opt_id(project)?.map(ProjectId),
        root_id: opt_id(root)?.map(RootId),
        operation_id: opt_id(operation)?.map(OperationId),
        aggregate_kind: txt(akind)?,
        aggregate_id: txt(aid)?,
        aggregate_revision: int(arev)?,
        kind: txt(kind)?,
        created_at_ms: int(created)?,
        payload: json_of(&txt(payload)?)?,
    })
}

pub(super) fn event(conn: &Connection, id: &EventId) -> Result<Option<EventRecord>, StorageError> {
    let raw = db(stmt(
        conn,
        &format!("SELECT {EVENT_COLUMNS} FROM events WHERE id=?1"),
    )?
    .query_row([id.0.as_str()], event_row)
    .optional())?;
    raw.map(event_of).transpose()
}

pub(super) fn page(
    conn: &Connection,
    filter: &EventFilter,
    after: i64,
    max: u16,
) -> Result<EventPage, StorageError> {
    let n = page_limit(max)?;
    if after < 0 {
        return Err(invalid("after must be non-negative"));
    }
    let watermark = committed_sequence(conn)?;
    if after > watermark {
        // A position beyond this store's committed history (another store/epoch or a forged
        // cursor) cannot be resumed.
        return Err(StorageError::CursorExpired);
    }
    let base = format!("SELECT {EVENT_COLUMNS} FROM events WHERE seq>?1");
    let raw: Vec<[Option<String>; 12]> = match filter {
        EventFilter::All => {
            let mut s = stmt(conn, &format!("{base} ORDER BY seq LIMIT ?2"))?;
            db(db(s.query_map(params![after, n + 1], event_row))?.collect())?
        }
        EventFilter::Aggregate { kind, id } => {
            let mut s = stmt(
                conn,
                &format!("{base} AND aggregate_kind=?3 AND aggregate_id=?4 ORDER BY seq LIMIT ?2"),
            )?;
            db(db(s.query_map(params![after, n + 1, kind, id], event_row))?.collect())?
        }
        EventFilter::Root(root) => {
            let mut s = stmt(
                conn,
                &format!("{base} AND root_id=?3 ORDER BY seq LIMIT ?2"),
            )?;
            db(db(s.query_map(params![after, n + 1, root.0.as_str()], event_row))?.collect())?
        }
    };
    let mut events = raw
        .into_iter()
        .map(event_of)
        .collect::<Result<Vec<_>, _>>()?;
    let has_more = events.len() as i64 > n;
    events.truncate(n as usize);
    let next_after = if has_more {
        events.last().map_or(after, |e| e.sequence)
    } else {
        watermark
    };
    Ok(EventPage {
        events,
        next_after,
        has_more,
        watermark,
    })
}

/// The history view `authority_history` hands out: the same appends without the refusal of
/// authority-history records. Only a caller holding the capability gets one.
pub(crate) struct AuthorityHistory<'c> {
    pub(super) conn: &'c Connection,
    pub(super) now: i64,
}

impl HistoryRead for AuthorityHistory<'_> {
    fn event(&self, id: &EventId) -> Result<Option<EventRecord>, StorageError> {
        event(self.conn, id)
    }
    fn page(
        &self,
        filter: &EventFilter,
        after: i64,
        limit: u16,
    ) -> Result<EventPage, StorageError> {
        page(self.conn, filter, after, limit)
    }
}

impl HistoryLog for AuthorityHistory<'_> {
    fn append(&mut self, e: NewEvent) -> Result<i64, StorageError> {
        append_event(self.conn, self.now, e)
    }
}

impl HistoryLog for Unit<'_> {
    fn append(&mut self, e: NewEvent) -> Result<i64, StorageError> {
        if devworks_core::authority::is_authority_history(&e) {
            return Err(devworks_core::authority::authority_history_refused());
        }
        append_event(self.conn, self.now, e)
    }
}

pub(super) fn append_event(conn: &Connection, now: i64, e: NewEvent) -> Result<i64, StorageError> {
    bounded("aggregate kind", &e.aggregate_kind, 128)?;
    bounded("aggregate id", &e.aggregate_id, 256)?;
    bounded("event kind", &e.kind, 128)?;
    object("event payload", &e.payload)?;
    if e.aggregate_revision <= 0 {
        return Err(invalid("aggregate revision must be positive"));
    }
    if actor_kind(conn, &e.actor)?.is_none() {
        return Err(StorageError::NotFound {
            kind: "actor",
            id: e.actor.0.as_str().to_owned(),
        });
    }
    require(
        conn,
        "projects",
        "project",
        e.project_id.as_ref().map(|p| p.0.as_str()),
    )?;
    require(
        conn,
        "work_items",
        "root",
        e.root_id.as_ref().map(|r| r.0.as_str()),
    )?;
    require(
        conn,
        "operations",
        "operation",
        e.operation_id.as_ref().map(|o| o.0.as_str()),
    )?;
    if exists(conn, "SELECT 1 FROM events WHERE id=?1", e.id.0.as_str())? {
        return Err(StorageError::Duplicate {
            kind: "event",
            id: e.id.0.as_str().to_owned(),
        });
    }
    db(stmt(
        conn,
        "INSERT INTO events(id,actor_id,project_id,root_id,operation_id,\
            aggregate_kind,aggregate_id,aggregate_revision,kind,created_at,\
            payload_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
    )?
    .execute(params![
        e.id.0.as_str(),
        e.actor.0.as_str(),
        e.project_id.as_ref().map(|p| p.0.as_str()),
        e.root_id.as_ref().map(|r| r.0.as_str()),
        e.operation_id.as_ref().map(|o| o.0.as_str()),
        e.aggregate_kind,
        e.aggregate_id,
        e.aggregate_revision,
        e.kind,
        now,
        // History is persisted and exported: credential-shaped content in free text is
        // redacted before it is written.
        text(&devworks_core::authority::redact::Redactor::new().redact_json(&e.payload))
    ]))?;
    Ok(conn.last_insert_rowid())
}
