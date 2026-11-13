//! Watchers and their delivery events (migration 50).
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, OperationId, ProjectId};
use rusqlite::{Connection, Row, params, params_from_iter};

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn corrupt(message: impl Into<String>) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, message)
}
fn id(value: String) -> Result<Id, StorageError> {
    Id::new(value).map_err(|_| corrupt("invalid watcher identifier"))
}
fn page_limit(limit: u16) -> Result<i64, StorageError> {
    if limit == 0 || limit > MAX_PAGE {
        return Err(invalid("watcher page limit out of bounds"));
    }
    Ok(i64::from(limit))
}

const WATCHER_COLUMNS: &str = "id,revision,phase,source_kind,project_id,session_id,actor_id,process_operation_id,executable_ref,tail_bytes,outcome_kind,exit_code,signal,outcome_detail,duration_ms,output_tail,output_truncated,stop_requested,created_at,updated_at,settled_at,probe_job_operation_id,probe_interval_ms,probe_exit_code,probe_output_pattern,probe_runs,probe_next_run_at,due_at,expires_at,match_pattern,match_each,match_min_interval_ms,match_scanned,match_pending,match_total,match_last_event_at,file_path";

#[allow(clippy::type_complexity)]
fn watcher_row(row: &Row<'_>) -> rusqlite::Result<WatcherRow> {
    Ok(WatcherRow {
        id: row.get(0)?,
        revision: row.get(1)?,
        phase: row.get(2)?,
        source_kind: row.get(3)?,
        project_id: row.get(4)?,
        session_id: row.get(5)?,
        actor_id: row.get(6)?,
        process_operation_id: row.get(7)?,
        executable_ref: row.get(8)?,
        tail_bytes: row.get(9)?,
        outcome_kind: row.get(10)?,
        exit_code: row.get(11)?,
        signal: row.get(12)?,
        outcome_detail: row.get(13)?,
        duration_ms: row.get(14)?,
        output_tail: row.get(15)?,
        output_truncated: row.get(16)?,
        stop_requested: row.get(17)?,
        created_at: row.get(18)?,
        updated_at: row.get(19)?,
        settled_at: row.get(20)?,
        probe_job_operation_id: row.get(21)?,
        probe_interval_ms: row.get(22)?,
        probe_exit_code: row.get(23)?,
        probe_output_pattern: row.get(24)?,
        probe_runs: row.get(25)?,
        probe_next_run_at: row.get(26)?,
        due_at: row.get(27)?,
        expires_at: row.get(28)?,
        match_pattern: row.get(29)?,
        match_each: row.get(30)?,
        match_min_interval_ms: row.get(31)?,
        match_scanned: row.get(32)?,
        match_pending: row.get(33)?,
        match_total: row.get(34)?,
        match_last_event_at: row.get(35)?,
        file_path: row.get(36)?,
    })
}
struct WatcherRow {
    id: String,
    revision: i64,
    phase: String,
    source_kind: String,
    project_id: Option<String>,
    session_id: String,
    actor_id: String,
    process_operation_id: Option<String>,
    executable_ref: Option<String>,
    tail_bytes: i64,
    outcome_kind: Option<String>,
    exit_code: Option<i64>,
    signal: Option<i64>,
    outcome_detail: Option<String>,
    duration_ms: Option<i64>,
    output_tail: Option<Vec<u8>>,
    output_truncated: bool,
    stop_requested: bool,
    created_at: i64,
    updated_at: i64,
    settled_at: Option<i64>,
    probe_job_operation_id: Option<String>,
    probe_interval_ms: Option<i64>,
    probe_exit_code: Option<i64>,
    probe_output_pattern: Option<String>,
    probe_runs: i64,
    probe_next_run_at: Option<i64>,
    due_at: Option<i64>,
    expires_at: Option<i64>,
    match_pattern: Option<String>,
    match_each: bool,
    match_min_interval_ms: Option<i64>,
    match_scanned: i64,
    match_pending: i64,
    match_total: i64,
    match_last_event_at: Option<i64>,
    file_path: Option<String>,
}
impl WatcherRow {
    fn into_stored(self) -> Result<StoredWatcher, StorageError> {
        Ok(StoredWatcher {
            id: id(self.id)?,
            revision: self.revision,
            phase: WatcherPhase::parse(&self.phase).ok_or_else(|| corrupt("watcher phase"))?,
            source_kind: self.source_kind,
            project_id: self.project_id.map(id).transpose()?.map(ProjectId),
            session_id: id(self.session_id)?,
            actor: ActorId(id(self.actor_id)?),
            process_operation_id: self
                .process_operation_id
                .map(id)
                .transpose()?
                .map(OperationId),
            executable_ref: self.executable_ref,
            tail_bytes: self.tail_bytes,
            outcome: self.outcome_kind.map(|kind| StoredWatchOutcome {
                kind,
                exit_code: self.exit_code,
                signal: self.signal,
                detail: self.outcome_detail,
            }),
            duration_ms: self.duration_ms,
            output_tail: self.output_tail,
            output_truncated: self.output_truncated,
            stop_requested: self.stop_requested,
            created_at_ms: self.created_at,
            updated_at_ms: self.updated_at,
            settled_at_ms: self.settled_at,
            probe: match (self.probe_job_operation_id, self.probe_interval_ms) {
                (Some(job), Some(interval_ms)) => Some(StoredProbe {
                    job_operation_id: OperationId(id(job)?),
                    interval_ms,
                    exit_code: self.probe_exit_code,
                    output_pattern: self.probe_output_pattern,
                    runs: self.probe_runs,
                    next_run_at_ms: self.probe_next_run_at,
                }),
                _ => None,
            },
            due_at_ms: self.due_at,
            expires_at_ms: self.expires_at,
            matching: self.match_pattern.map(|pattern| StoredMatch {
                pattern,
                each: self.match_each,
                min_interval_ms: self.match_min_interval_ms,
                scanned: self.match_scanned,
                pending: self.match_pending,
                total: self.match_total,
                last_event_at_ms: self.match_last_event_at,
            }),
            file_path: self.file_path,
        })
    }
}
fn watchers_where<P: rusqlite::Params>(
    conn: &Connection,
    tail: &str,
    params: P,
) -> Result<Vec<StoredWatcher>, StorageError> {
    let mut statement = db(conn.prepare_cached(&format!(
        "SELECT {WATCHER_COLUMNS} FROM watchers WHERE {tail}"
    )))?;
    let rows =
        db(db(statement.query_map(params, watcher_row))?.collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter().map(WatcherRow::into_stored).collect()
}

const EVENT_COLUMNS: &str = "id,watcher_id,session_id,project_id,state,payload_json,transcript_sequence,carried_turn_id,created_at,delivered_at,acknowledged_at,match_count";
fn events_where<P: rusqlite::Params>(
    conn: &Connection,
    tail: &str,
    params: P,
) -> Result<Vec<StoredWatchEvent>, StorageError> {
    let mut statement = db(conn.prepare_cached(&format!(
        "SELECT {EVENT_COLUMNS} FROM watcher_events WHERE {tail}"
    )))?;
    let rows = db(db(statement.query_map(params, |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<i64>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, Option<i64>>(9)?,
            row.get::<_, Option<i64>>(10)?,
            row.get::<_, Option<i64>>(11)?,
        ))
    }))?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter()
        .map(
            |(
                event,
                watcher,
                session,
                project,
                state,
                payload,
                sequence,
                turn,
                created,
                delivered,
                acknowledged,
                match_count,
            )| {
                Ok(StoredWatchEvent {
                    id: id(event)?,
                    watcher_id: id(watcher)?,
                    session_id: id(session)?,
                    project_id: project.map(id).transpose()?.map(ProjectId),
                    state: WatchEventState::parse(&state)
                        .ok_or_else(|| corrupt("watcher event state"))?,
                    payload: serde_json::from_str(&payload)
                        .map_err(|_| corrupt("invalid watcher event JSON"))?,
                    transcript_sequence: sequence,
                    carried_turn_id: turn.map(id).transpose()?,
                    created_at_ms: created,
                    delivered_at_ms: delivered,
                    acknowledged_at_ms: acknowledged,
                    match_count,
                })
            },
        )
        .collect()
}
fn placeholders(from: usize, count: usize) -> String {
    (from..from + count)
        .map(|n| format!("?{n}"))
        .collect::<Vec<_>>()
        .join(",")
}

macro_rules! watcher_reads {
    ($type:ty) => {
        impl WatcherRead for $type {
            fn watcher(&self, id: &Id) -> Result<Option<StoredWatcher>, StorageError> {
                Ok(watchers_where(self.conn, "id=?1", [id.as_str()])?
                    .into_iter()
                    .next())
            }
            fn watcher_for_process(
                &self,
                operation: &OperationId,
            ) -> Result<Option<StoredWatcher>, StorageError> {
                Ok(
                    watchers_where(self.conn, "process_operation_id=?1", [operation.0.as_str()])?
                        .into_iter()
                        .next(),
                )
            }
            fn watchers(
                &self,
                filter: &WatcherFilter,
                phases: &[WatcherPhase],
                before: Option<(i64, &Id)>,
                limit: u16,
            ) -> Result<Vec<StoredWatcher>, StorageError> {
                let limit = page_limit(limit)?;
                let mut values: Vec<rusqlite::types::Value> = Vec::new();
                let owner = match filter {
                    WatcherFilter::Session(session) => {
                        values.push(session.as_str().to_owned().into());
                        "session_id=?1"
                    }
                    WatcherFilter::Project(Some(project)) => {
                        values.push(project.0.as_str().to_owned().into());
                        "project_id=?1"
                    }
                    WatcherFilter::Project(None) => {
                        values.push(rusqlite::types::Value::Null);
                        "project_id IS NULL AND ?1 IS NULL"
                    }
                };
                values.push(before.map_or(rusqlite::types::Value::Null, |(at, _)| at.into()));
                values.push(before.map_or(rusqlite::types::Value::Null, |(_, b)| {
                    b.as_str().to_owned().into()
                }));
                values.push(limit.into());
                let mut tail = format!(
                    "{owner} AND (?2 IS NULL OR created_at<?2 OR (created_at=?2 AND id<?3))"
                );
                if !phases.is_empty() {
                    tail.push_str(&format!(
                        " AND phase IN ({})",
                        placeholders(5, phases.len())
                    ));
                    values.extend(phases.iter().map(|p| p.as_str().to_owned().into()));
                }
                tail.push_str(" ORDER BY created_at DESC,id DESC LIMIT ?4");
                watchers_where(self.conn, &tail, params_from_iter(values))
            }
            fn open_watchers(
                &self,
                after: Option<&Id>,
                limit: u16,
            ) -> Result<Vec<StoredWatcher>, StorageError> {
                watchers_where(
                    self.conn,
                    "phase IN ('Active','Stopping') AND id>?1 ORDER BY id LIMIT ?2",
                    params![after.map_or("", Id::as_str), page_limit(limit)?],
                )
            }
            fn pending_watch_events(
                &self,
                after: Option<(i64, &Id)>,
                limit: u16,
            ) -> Result<Vec<StoredWatchEvent>, StorageError> {
                let (at, id) = after.map_or((i64::MIN, ""), |(at, id)| (at, id.as_str()));
                events_where(
                    self.conn,
                    "state='Pending' AND (created_at>?1 OR (created_at=?1 AND id>?2)) \
                     ORDER BY created_at,id LIMIT ?3",
                    params![at, id, page_limit(limit)?],
                )
            }
            fn watch_event(&self, id: &Id) -> Result<Option<StoredWatchEvent>, StorageError> {
                Ok(events_where(self.conn, "id=?1", [id.as_str()])?
                    .into_iter()
                    .next())
            }
            fn watch_event_for_watcher(
                &self,
                watcher: &Id,
            ) -> Result<Option<StoredWatchEvent>, StorageError> {
                Ok(events_where(
                    self.conn,
                    "watcher_id=?1 AND match_count IS NULL",
                    [watcher.as_str()],
                )?
                .into_iter()
                .next())
            }
            fn watch_events(
                &self,
                session: &Id,
                states: &[WatchEventState],
                limit: u16,
            ) -> Result<Vec<StoredWatchEvent>, StorageError> {
                if states.is_empty() {
                    return Ok(Vec::new());
                }
                let mut values: Vec<rusqlite::types::Value> = vec![
                    session.as_str().to_owned().into(),
                    page_limit(limit)?.into(),
                ];
                values.extend(states.iter().map(|s| s.as_str().to_owned().into()));
                events_where(
                    self.conn,
                    &format!(
                        "session_id=?1 AND state IN ({}) ORDER BY created_at,id LIMIT ?2",
                        placeholders(3, states.len())
                    ),
                    params_from_iter(values),
                )
            }
        }
    };
}
watcher_reads!(Snapshot<'_>);
watcher_reads!(Unit<'_>);

fn check(watcher: &StoredWatcher) -> Result<(), StorageError> {
    if watcher.revision < 1
        || watcher.created_at_ms < 0
        || watcher.updated_at_ms < watcher.created_at_ms
    {
        return Err(invalid(
            "watcher revision and times must be ordered and nonnegative",
        ));
    }
    if !(0..=MAX_WATCH_TAIL_BYTES as i64).contains(&watcher.tail_bytes)
        || watcher
            .output_tail
            .as_ref()
            .is_some_and(|tail| tail.len() > MAX_WATCH_TAIL_BYTES)
    {
        return Err(invalid("watcher output tail exceeds its bound"));
    }
    Ok(())
}
fn write_params(w: &StoredWatcher) -> Vec<rusqlite::types::Value> {
    use rusqlite::types::Value;
    let text = |s: &str| Value::Text(s.to_owned());
    let opt = |s: Option<&str>| s.map_or(Value::Null, |s| Value::Text(s.to_owned()));
    let int = |n: Option<i64>| n.map_or(Value::Null, Value::Integer);
    vec![
        text(w.id.as_str()),
        Value::Integer(w.revision),
        text(w.phase.as_str()),
        text(&w.source_kind),
        opt(w.project_id.as_ref().map(|p| p.0.as_str())),
        text(w.session_id.as_str()),
        text(w.actor.0.as_str()),
        opt(w.process_operation_id.as_ref().map(|o| o.0.as_str())),
        opt(w.executable_ref.as_deref()),
        Value::Integer(w.tail_bytes),
        opt(w.outcome.as_ref().map(|o| o.kind.as_str())),
        int(w.outcome.as_ref().and_then(|o| o.exit_code)),
        int(w.outcome.as_ref().and_then(|o| o.signal)),
        opt(w.outcome.as_ref().and_then(|o| o.detail.as_deref())),
        int(w.duration_ms),
        w.output_tail.clone().map_or(Value::Null, Value::Blob),
        Value::Integer(i64::from(w.output_truncated)),
        Value::Integer(i64::from(w.stop_requested)),
        Value::Integer(w.created_at_ms),
        Value::Integer(w.updated_at_ms),
        int(w.settled_at_ms),
        opt(w.probe.as_ref().map(|p| p.job_operation_id.0.as_str())),
        int(w.probe.as_ref().map(|p| p.interval_ms)),
        int(w.probe.as_ref().and_then(|p| p.exit_code)),
        opt(w.probe.as_ref().and_then(|p| p.output_pattern.as_deref())),
        Value::Integer(w.probe.as_ref().map_or(0, |p| p.runs)),
        int(w.probe.as_ref().and_then(|p| p.next_run_at_ms)),
        int(w.due_at_ms),
        int(w.expires_at_ms),
        opt(w.matching.as_ref().map(|m| m.pattern.as_str())),
        Value::Integer(i64::from(w.matching.as_ref().is_some_and(|m| m.each))),
        int(w.matching.as_ref().and_then(|m| m.min_interval_ms)),
        Value::Integer(w.matching.as_ref().map_or(0, |m| m.scanned)),
        Value::Integer(w.matching.as_ref().map_or(0, |m| m.pending)),
        Value::Integer(w.matching.as_ref().map_or(0, |m| m.total)),
        int(w.matching.as_ref().and_then(|m| m.last_event_at_ms)),
        opt(w.file_path.as_deref()),
    ]
}

impl WatcherRepository for Unit<'_> {
    fn insert_watcher(&mut self, watcher: &StoredWatcher) -> Result<(), StorageError> {
        check(watcher)?;
        if watcher.revision != 1 {
            return Err(invalid("a new watcher starts at revision 1"));
        }
        if self.watcher(&watcher.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "watcher",
                id: watcher.id.as_str().into(),
            });
        }
        db(self.conn.execute(
            &format!(
                "INSERT INTO watchers({WATCHER_COLUMNS}) VALUES({})",
                placeholders(1, 37)
            ),
            params_from_iter(write_params(watcher)),
        ))?;
        Ok(())
    }
    fn update_watcher(&mut self, watcher: &StoredWatcher) -> Result<(), StorageError> {
        check(watcher)?;
        let mut values = write_params(watcher);
        values.push(rusqlite::types::Value::Integer(watcher.revision - 1));
        let changed = db(self.conn.execute(
            "UPDATE watchers SET revision=?2,phase=?3,source_kind=?4,project_id=?5,session_id=?6,actor_id=?7,process_operation_id=?8,executable_ref=?9,tail_bytes=?10,outcome_kind=?11,exit_code=?12,signal=?13,outcome_detail=?14,duration_ms=?15,output_tail=?16,output_truncated=?17,stop_requested=?18,created_at=?19,updated_at=?20,settled_at=?21,probe_job_operation_id=?22,probe_interval_ms=?23,probe_exit_code=?24,probe_output_pattern=?25,probe_runs=?26,probe_next_run_at=?27,due_at=?28,expires_at=?29,match_pattern=?30,match_each=?31,match_min_interval_ms=?32,match_scanned=?33,match_pending=?34,match_total=?35,match_last_event_at=?36,file_path=?37 WHERE id=?1 AND revision=?38",
            params_from_iter(values),
        ))?;
        if changed != 1 {
            return Err(StorageError::LeaseLost {
                kind: "watcher",
                id: watcher.id.as_str().into(),
            });
        }
        Ok(())
    }
    fn insert_watch_event(&mut self, event: &StoredWatchEvent) -> Result<(), StorageError> {
        if event.state != WatchEventState::Pending || event.created_at_ms < 0 {
            return Err(invalid("a new watcher event is pending"));
        }
        if event.match_count.is_some_and(|n| n < 1) {
            return Err(invalid("a match event counts at least one match"));
        }
        if event.match_count.is_none() && self.watch_event_for_watcher(&event.watcher_id)?.is_some()
        {
            return Err(StorageError::Duplicate {
                kind: "watcher_event",
                id: event.watcher_id.as_str().into(),
            });
        }
        let payload = serde_json::to_string(&event.payload)
            .map_err(|_| invalid("watcher event cannot be serialized"))?;
        db(self.conn.execute(
            "INSERT INTO watcher_events(id,watcher_id,session_id,project_id,state,payload_json,created_at,match_count) VALUES(?1,?2,?3,?4,'Pending',?5,?6,?7)",
            params![
                event.id.as_str(),
                event.watcher_id.as_str(),
                event.session_id.as_str(),
                event.project_id.as_ref().map(|p| p.0.as_str()),
                payload,
                event.created_at_ms,
                event.match_count
            ],
        ))?;
        Ok(())
    }
    fn deliver_watch_event(
        &mut self,
        id: &Id,
        transcript_sequence: i64,
        at_ms: i64,
    ) -> Result<bool, StorageError> {
        let changed = db(self.conn.execute(
            "UPDATE watcher_events SET state='Delivered',transcript_sequence=?2,delivered_at=?3 WHERE id=?1 AND state='Pending'",
            params![id.as_str(), transcript_sequence, at_ms],
        ))?;
        Ok(changed == 1)
    }
    fn carry_watch_events(&mut self, session: &Id, turn: &Id) -> Result<u64, StorageError> {
        Ok(db(self.conn.execute(
            "UPDATE watcher_events SET carried_turn_id=?2 WHERE session_id=?1 AND state='Delivered'",
            params![session.as_str(), turn.as_str()],
        ))? as u64)
    }
    fn release_watch_events(
        &mut self,
        session: &Id,
        turn: &Id,
        completed: bool,
        at_ms: i64,
    ) -> Result<u64, StorageError> {
        let sql = if completed {
            "UPDATE watcher_events SET state='Acknowledged',carried_turn_id=NULL,acknowledged_at=?3 WHERE session_id=?1 AND carried_turn_id=?2"
        } else {
            "UPDATE watcher_events SET carried_turn_id=NULL WHERE session_id=?1 AND carried_turn_id=?2 AND ?3 IS NOT NULL"
        };
        Ok(db(self
            .conn
            .execute(sql, params![session.as_str(), turn.as_str(), at_ms]))?
            as u64)
    }
}
