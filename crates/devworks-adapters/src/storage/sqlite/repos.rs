//! Repository implementations. All SQL lives here and in the migrations; callers see only the
//! core port traits. Read operations are free functions over `&Connection` shared by snapshots
//! and the write unit; write operations exist only on [`Unit`], which borrows the writer's open
//! transaction.
use super::conn::db;
use devworks_core::context::{PrincipalKind, ReceiptIdentity};
use devworks_core::fingerprint::Fingerprint;
use devworks_core::ports::storage::*;
use devworks_core::ports::{GrantUse, IdKind, IdSource, StoredReceipt};
use devworks_core::protocol::results::DomainDecision;
use devworks_core::protocol::{
    ActorId, AssignmentId, AttemptId, CONTRACT, CommandId, EventId, ExecutionId, Id, OperationId,
    OperationState, ProjectId, Receipt, Revision, RevisionDimension, RootId, StoreId,
};
use devworks_core::registry::SchemaRevision;
use rusqlite::{CachedStatement, Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Value helpers

fn invalid(m: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(m.into())
}
fn corrupt(m: impl Into<String>) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, m)
}
fn id_of(s: String) -> Result<Id, StorageError> {
    Id::new(s).map_err(|e| corrupt(format!("stored identifier: {e}")))
}
fn opt_id(s: Option<String>) -> Result<Option<Id>, StorageError> {
    s.map(id_of).transpose()
}
fn json_of(s: &str) -> Result<Value, StorageError> {
    serde_json::from_str(s).map_err(|e| corrupt(format!("stored JSON: {e}")))
}
fn text(v: &Value) -> String {
    v.to_string()
}
fn bounded(field: &str, s: &str, max: usize) -> Result<(), StorageError> {
    if s.is_empty() || s.len() > max || s.contains('\0') {
        return Err(invalid(format!("{field} must be 1..={max} bytes")));
    }
    Ok(())
}
fn object(field: &str, v: &Value) -> Result<(), StorageError> {
    if !v.is_object() {
        return Err(invalid(format!("{field} must be a JSON object")));
    }
    Ok(())
}
fn limit(l: u16) -> Result<i64, StorageError> {
    if l == 0 || l > MAX_PAGE {
        return Err(invalid(format!("limit must be 1..={MAX_PAGE}")));
    }
    Ok(i64::from(l))
}
fn revision(v: i64) -> Result<Revision, StorageError> {
    Revision::new(v).map_err(|_| corrupt("non-positive stored revision"))
}
fn stmt<'c>(conn: &'c Connection, sql: &str) -> Result<CachedStatement<'c>, StorageError> {
    db(conn.prepare_cached(sql))
}
fn exists(conn: &Connection, sql: &str, id: &str) -> Result<bool, StorageError> {
    db(stmt(conn, sql)?.exists([id]))
}
fn require(
    conn: &Connection,
    table: &'static str,
    kind: &'static str,
    id: Option<&str>,
) -> Result<(), StorageError> {
    let Some(id) = id else { return Ok(()) };
    let sql = format!("SELECT 1 FROM {table} WHERE id=?1");
    if exists(conn, &sql, id)? {
        Ok(())
    } else {
        Err(StorageError::NotFound {
            kind,
            id: id.to_owned(),
        })
    }
}

fn state_name(s: OperationState) -> &'static str {
    use OperationState::*;
    match s {
        IntentRecorded => "IntentRecorded",
        Preflighted => "Preflighted",
        Dispatching => "Dispatching",
        ObservedComplete => "ObservedComplete",
        Failed => "Failed",
        OutcomeUnknown => "OutcomeUnknown",
        NeedsInput => "NeedsInput",
        RetryPending => "RetryPending",
        CancelledBeforeDispatch => "CancelledBeforeDispatch",
    }
}
fn parse_state(s: &str) -> Result<OperationState, StorageError> {
    serde_json::from_value(Value::String(s.to_owned()))
        .map_err(|_| corrupt(format!("unknown operation state {s}")))
}
fn queue_state_name(s: QueueState) -> &'static str {
    match s {
        QueueState::Ready => "Ready",
        QueueState::Claimed => "Claimed",
        QueueState::Completed => "Completed",
        QueueState::Failed => "Failed",
        QueueState::Cancelled => "Cancelled",
        QueueState::Reconciling => "Reconciling",
        QueueState::Dismissed => "Dismissed",
    }
}
fn parse_queue_state(s: &str) -> Result<QueueState, StorageError> {
    Ok(match s {
        "Ready" => QueueState::Ready,
        "Claimed" => QueueState::Claimed,
        "Completed" => QueueState::Completed,
        "Failed" => QueueState::Failed,
        "Cancelled" => QueueState::Cancelled,
        "Reconciling" => QueueState::Reconciling,
        "Dismissed" => QueueState::Dismissed,
        other => return Err(corrupt(format!("unknown queue state {other}"))),
    })
}
pub(crate) fn principal_name(k: PrincipalKind) -> &'static str {
    match k {
        PrincipalKind::Human => "human",
        PrincipalKind::Agent => "agent",
        PrincipalKind::Package => "package",
        PrincipalKind::McpClient => "mcp_client",
        PrincipalKind::Schedule => "schedule",
        PrincipalKind::Host => "host",
    }
}
pub(crate) fn parse_principal(s: &str) -> Result<PrincipalKind, StorageError> {
    Ok(match s {
        "human" => PrincipalKind::Human,
        "agent" => PrincipalKind::Agent,
        "package" => PrincipalKind::Package,
        "mcp_client" => PrincipalKind::McpClient,
        "schedule" => PrincipalKind::Schedule,
        "host" => PrincipalKind::Host,
        other => return Err(corrupt(format!("unknown actor kind {other}"))),
    })
}

// ---------------------------------------------------------------------------------------------
// Reads shared by snapshots and the write unit

pub(crate) fn committed_sequence(conn: &Connection) -> Result<i64, StorageError> {
    db(stmt(conn, "SELECT coalesce(max(seq),0) FROM events")?.query_row([], |r| r.get(0)))
}

pub(crate) fn stamp(conn: &Connection) -> Result<SnapshotStamp, StorageError> {
    let (store_id, restore_epoch): (String, i64) = db(stmt(
        conn,
        "SELECT store_id,restore_epoch FROM store_meta WHERE singleton=1",
    )?
    .query_row([], |r| Ok((r.get(0)?, r.get(1)?))))?;
    Ok(SnapshotStamp {
        store_id: StoreId(id_of(store_id)?),
        restore_epoch,
        committed_sequence: committed_sequence(conn)?,
    })
}

pub(crate) fn actor_kind(
    conn: &Connection,
    actor: &ActorId,
) -> Result<Option<PrincipalKind>, StorageError> {
    let kind: Option<String> = db(stmt(conn, "SELECT kind FROM actors WHERE id=?1")?
        .query_row([actor.0.as_str()], |r| r.get(0))
        .optional())?;
    kind.as_deref().map(parse_principal).transpose()
}

fn receipt_reply(
    conn: &Connection,
    identity: &ReceiptIdentity,
) -> Result<Option<ReceiptReply>, StorageError> {
    let row: Option<(String, String, String, String, i64)> = db(stmt(
        conn,
        "SELECT command_id,artifact_id,version_id,schema_json,published_at FROM receipt_replies WHERE actor_id=?1 AND scope_key=?2 AND idempotency_key=?3",
    )?
    .query_row(
        params![
            identity.actor_id().0.as_str(),
            identity.scope_key().as_str(),
            identity.request_key().0.as_str()
        ],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )
    .optional())?;
    let Some((command, artifact, version, schema, at)) = row else {
        return Ok(None);
    };
    Ok(Some(ReceiptReply {
        command_id: CommandId(id_of(command)?),
        artifact_id: id_of(artifact)?,
        version_id: id_of(version)?,
        schema: json_of(&schema)?,
        published_at_ms: at,
    }))
}

fn lookup_receipt(
    conn: &Connection,
    identity: &ReceiptIdentity,
) -> Result<Option<LedgerEntry>, StorageError> {
    let row: Option<(String, String, String)> = db(stmt(conn,
            "SELECT command_kind,payload_digest,result_json FROM command_receipts WHERE actor_id=?1 AND scope_key=?2 AND idempotency_key=?3",
        )?
        .query_row(
            params![
                identity.actor_id().0.as_str(),
                identity.scope_key().as_str(),
                identity.request_key().0.as_str()
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional())?;
    let Some((operation, digest, result)) = row else {
        return Ok(None);
    };
    let stored = json_of(&result)?;
    let contract = stored["contract"].as_str().unwrap_or_default().to_owned();
    let schema_revision = SchemaRevision(
        stored["schemaRevision"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0),
    );
    let fingerprint = Fingerprint::from_hex(&digest);
    let incompatible = |reason: String| LedgerEntry::Incompatible {
        operation: operation.clone(),
        contract: contract.clone(),
        schema_revision,
        fingerprint: fingerprint.clone(),
        reason,
    };
    if contract != CONTRACT {
        return Ok(Some(incompatible(format!(
            "receipt recorded under contract {contract:?}"
        ))));
    }
    let Some(fp) = fingerprint.clone() else {
        return Ok(Some(incompatible(
            "receipt digest is not a fingerprint".into(),
        )));
    };
    match serde_json::from_value::<Receipt<DomainDecision>>(stored["receipt"].clone()) {
        Ok(receipt) => Ok(Some(LedgerEntry::Current(StoredReceipt {
            operation,
            contract,
            schema_revision,
            fingerprint: fp,
            receipt,
        }))),
        Err(e) => Ok(Some(incompatible(format!("receipt does not decode: {e}")))),
    }
}

pub(crate) const EVENT_COLUMNS: &str = "seq,id,actor_id,project_id,root_id,operation_id,aggregate_kind,aggregate_id,aggregate_revision,kind,created_at,payload_json";
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
fn event(conn: &Connection, id: &EventId) -> Result<Option<EventRecord>, StorageError> {
    let raw = db(stmt(
        conn,
        &format!("SELECT {EVENT_COLUMNS} FROM events WHERE id=?1"),
    )?
    .query_row([id.0.as_str()], event_row)
    .optional())?;
    raw.map(event_of).transpose()
}
fn page(
    conn: &Connection,
    filter: &EventFilter,
    after: i64,
    max: u16,
) -> Result<EventPage, StorageError> {
    let n = limit(max)?;
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

const OP_COLUMNS: &str = "id,root_id,assignment_id,capability,schema_version,args_digest,payload_json,state,revision,created_at";
fn attempts(conn: &Connection, op: &str) -> Result<Vec<AttemptRecord>, StorageError> {
    let mut stmt = stmt(
        conn,
        "SELECT id,attempt_no,boot_id,execution_id,owner_generation,control_revision,policy_revision,state,observations_json FROM operation_attempts WHERE operation_id=?1 ORDER BY attempt_no",
    )?;
    type Raw = (
        String,
        i64,
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        String,
        String,
    );
    let rows: Vec<Raw> = db(db(stmt.query_map([op], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
            r.get(7)?,
            r.get(8)?,
        ))
    }))?
    .collect())?;
    rows.into_iter()
        .map(|(id, no, boot, exec, gen_, ctrl, pol, state, obs)| {
            let observations = match json_of(&obs)? {
                Value::Array(a) => a,
                _ => return Err(corrupt("observations are not an array")),
            };
            Ok(AttemptRecord {
                id: AttemptId(id_of(id)?),
                attempt_no: no,
                boot_id: id_of(boot)?,
                binding: AttemptBinding {
                    execution_id: opt_id(exec)?.map(ExecutionId),
                    owner_generation: gen_,
                    control_revision: ctrl,
                    policy_revision: pol,
                },
                state: parse_state(&state)?,
                observations,
            })
        })
        .collect()
}
type OpRaw = (
    String,
    Option<String>,
    Option<String>,
    String,
    i64,
    String,
    String,
    String,
    i64,
    i64,
);
fn op_row(r: &Row<'_>) -> rusqlite::Result<OpRaw> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
    ))
}
fn op_of(conn: &Connection, raw: OpRaw) -> Result<OperationRecord, StorageError> {
    let (
        id,
        root,
        assignment,
        capability,
        schema_version,
        args_digest,
        payload,
        state,
        rev,
        created,
    ) = raw;
    let attempts = attempts(conn, &id)?;
    Ok(OperationRecord {
        id: OperationId(id_of(id)?),
        root_id: opt_id(root)?.map(RootId),
        assignment_id: opt_id(assignment)?.map(AssignmentId),
        capability,
        schema_version,
        args_digest,
        payload: json_of(&payload)?,
        state: parse_state(&state)?,
        revision: rev,
        created_at_ms: created,
        attempts,
    })
}
fn operation(conn: &Connection, id: &OperationId) -> Result<Option<OperationRecord>, StorageError> {
    let raw = db(stmt(
        conn,
        &format!("SELECT {OP_COLUMNS} FROM operations WHERE id=?1"),
    )?
    .query_row([id.0.as_str()], op_row)
    .optional())?;
    raw.map(|r| op_of(conn, r)).transpose()
}
fn unresolved(conn: &Connection, max: u16) -> Result<Vec<OperationRecord>, StorageError> {
    let n = limit(max)?;
    let mut s = stmt(
        conn,
        &format!(
            "SELECT {OP_COLUMNS} FROM operations WHERE state NOT IN ('ObservedComplete','Failed','CancelledBeforeDispatch') ORDER BY id LIMIT ?1"
        ),
    )?;
    let raws: Vec<OpRaw> = db(db(s.query_map([n], op_row))?.collect())?;
    raws.into_iter().map(|r| op_of(conn, r)).collect()
}

const QUEUE_COLUMNS: &str = "id,consumer,event_id,root_id,operation_id,priority,ready_at,state,owner_id,lease_until,generation,cancellation_epoch,attempt_count,payload_json,partition_key,event_seq,revision,last_error,claim_boot_id,cancel_requested,failed_at,awaiting_eligibility";
/// SQL list of settled queue states (they never block a partition or the watermark).
/// `Compacted` is a dedupe tombstone of a settled delivery.
const SETTLED: &str = "('Completed','Cancelled','Dismissed','Compacted')";
struct QueueRaw {
    id: String,
    consumer: String,
    event: String,
    root: Option<String>,
    op: Option<String>,
    priority: i64,
    ready: i64,
    state: String,
    owner: Option<String>,
    lease: Option<i64>,
    generation: i64,
    epoch: i64,
    attempts: i64,
    payload: String,
    partition: Option<String>,
    seq: i64,
    revision: i64,
    error: Option<String>,
    claim_boot: Option<String>,
    cancel_requested: bool,
    failed_at: Option<i64>,
    awaiting_eligibility: bool,
}
fn queue_row(r: &Row<'_>) -> rusqlite::Result<QueueRaw> {
    Ok(QueueRaw {
        id: r.get(0)?,
        consumer: r.get(1)?,
        event: r.get(2)?,
        root: r.get(3)?,
        op: r.get(4)?,
        priority: r.get(5)?,
        ready: r.get(6)?,
        state: r.get(7)?,
        owner: r.get(8)?,
        lease: r.get(9)?,
        generation: r.get(10)?,
        epoch: r.get(11)?,
        attempts: r.get(12)?,
        payload: r.get(13)?,
        partition: r.get(14)?,
        seq: r.get(15)?,
        revision: r.get(16)?,
        error: r.get(17)?,
        claim_boot: r.get(18)?,
        cancel_requested: r.get(19)?,
        failed_at: r.get(20)?,
        awaiting_eligibility: r.get(21)?,
    })
}
fn queue_of(r: QueueRaw) -> Result<QueueEntry, StorageError> {
    Ok(QueueEntry {
        id: id_of(r.id)?,
        consumer: r.consumer,
        event_id: EventId(id_of(r.event)?),
        root_id: opt_id(r.root)?.map(RootId),
        operation_id: opt_id(r.op)?.map(OperationId),
        priority: r.priority,
        ready_at_ms: r.ready,
        state: parse_queue_state(&r.state)?,
        owner: opt_id(r.owner)?,
        lease_until_ms: r.lease,
        generation: r.generation,
        cancellation_epoch: r.epoch,
        attempt_count: r.attempts,
        payload: json_of(&r.payload)?,
        partition: r.partition,
        event_sequence: r.seq,
        revision: r.revision,
        last_error: r.error,
        claim_boot_id: opt_id(r.claim_boot)?,
        cancel_requested: r.cancel_requested,
        failed_at_ms: r.failed_at,
        awaiting_eligibility: r.awaiting_eligibility,
    })
}
fn entry(conn: &Connection, id: &Id) -> Result<Option<QueueEntry>, StorageError> {
    let raw = db(stmt(
        conn,
        &format!("SELECT {QUEUE_COLUMNS} FROM queue_entries WHERE id=?1 AND state<>'Compacted'"),
    )?
    .query_row([id.as_str()], queue_row)
    .optional())?;
    raw.map(queue_of).transpose()
}
fn delivery(
    conn: &Connection,
    consumer: &str,
    event: &EventId,
) -> Result<Option<QueueEntry>, StorageError> {
    let raw = db(stmt(
        conn,
        &format!(
            "SELECT {QUEUE_COLUMNS} FROM queue_entries WHERE consumer=?1 AND event_id=?2 AND state<>'Compacted'"
        ),
    )?
    .query_row(params![consumer, event.0.as_str()], queue_row)
    .optional())?;
    raw.map(queue_of).transpose()
}
fn status(conn: &Connection, consumer: &str) -> Result<QueueStatus, StorageError> {
    let mut s = stmt(
        conn,
        "SELECT state,count(*),min(ready_at) FROM queue_entries WHERE consumer=?1 AND state<>'Compacted' GROUP BY state",
    )?;
    let rows: Vec<(String, i64, Option<i64>)> =
        db(db(s.query_map([consumer], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))?.collect())?;
    let mut out = QueueStatus::default();
    for (state, count, oldest) in rows {
        match parse_queue_state(&state)? {
            QueueState::Ready => {
                out.ready = count;
                out.oldest_ready_at_ms = oldest;
            }
            QueueState::Claimed => out.claimed = count,
            QueueState::Reconciling => out.reconciling = count,
            QueueState::Failed => out.failed = count,
            QueueState::Completed => out.completed = count,
            QueueState::Cancelled => out.cancelled = count,
            QueueState::Dismissed => out.dismissed = count,
        }
    }
    Ok(out)
}
/// Inspection ranks: (rank, state predicate, keyset predicate, order).
const INSPECTION_RANKS: [(u8, &str, &str, &str); 3] = [
    (
        0,
        "state='Claimed'",
        "(event_seq,id)>(?3,?5)",
        "event_seq,id",
    ),
    (
        1,
        "state IN ('Failed','Reconciling')",
        "(coalesce(failed_at,0),id)<(?3,?5)",
        "coalesce(failed_at,0) DESC,id DESC",
    ),
    (
        2,
        "state='Ready'",
        "(-priority,ready_at,id)>(-?3,?4,?5)",
        "priority DESC,ready_at,id",
    ),
];
fn deliveries(
    conn: &Connection,
    filter: &DeliveryFilter,
    after: Option<&DeliveryKey>,
    max: u16,
) -> Result<DeliveryPage, StorageError> {
    let n = limit(max)?;
    // One extra row tells whether more follow. Ranks are read in order, each with its own
    // keyset predicate, so the combined order is total and stable across pages.
    let mut entries = Vec::new();
    for (rank, predicate, keyset, order) in INSPECTION_RANKS {
        let start = match after {
            Some(k) if k.rank > rank => continue,
            Some(k) if k.rank == rank => Some(k),
            _ => None,
        };
        let want = n + 1 - entries.len() as i64;
        if want <= 0 {
            break;
        }
        let sql = format!(
            "SELECT {QUEUE_COLUMNS} FROM queue_entries WHERE {predicate} AND (?1 IS NULL OR consumer=?1) AND (?2 IS NULL OR operation_id=?2) AND ({}) ORDER BY {order} LIMIT ?6",
            if start.is_some() {
                keyset
            } else {
                "?3 IS NULL AND ?4 IS NULL AND ?5 IS NULL"
            }
        );
        let (a, b, id) = match start {
            Some(k) => (Some(k.a), Some(k.b), Some(k.id.as_str().to_owned())),
            None => (None, None, None),
        };
        let mut s = stmt(conn, &sql)?;
        let raws: Vec<QueueRaw> = db(db(s.query_map(
            params![
                filter.consumer,
                filter.operation_id.as_ref().map(|o| o.0.as_str()),
                a,
                b,
                id,
                want
            ],
            queue_row,
        ))?
        .collect())?;
        for raw in raws {
            entries.push(queue_of(raw)?);
        }
    }
    let next = if entries.len() as i64 > n {
        entries.truncate(n as usize);
        entries.last().and_then(QueueEntry::inspection_key)
    } else {
        None
    };
    Ok(DeliveryPage { entries, next })
}
fn progress_row(
    conn: &Connection,
    consumer: &str,
    partition: &str,
) -> Result<(i64, i64), StorageError> {
    Ok(db(stmt(
        conn,
        "SELECT applied_seq,revision FROM consumer_progress WHERE consumer=?1 AND partition_key=?2",
    )?
    .query_row(params![consumer, partition], |r| Ok((r.get(0)?, r.get(1)?)))
    .optional())?
    .unwrap_or((0, 0)))
}
fn progress(conn: &Connection, consumer: &str) -> Result<ConsumerProgress, StorageError> {
    let (highest_settled, revision) = progress_row(conn, consumer, "")?;
    let (lowest_unsettled, next_retry): (Option<i64>, Option<i64>) = db(stmt(
        conn,
        &format!(
            "SELECT min(event_seq),min(CASE WHEN state='Ready' AND attempt_count>0 THEN ready_at END) FROM queue_entries WHERE consumer=?1 AND state NOT IN {SETTLED}"
        ),
    )?
    .query_row(params![consumer], |r| Ok((r.get(0)?, r.get(1)?))))?;
    let watermark = match lowest_unsettled {
        Some(low) => highest_settled.min(low - 1).max(0),
        None => highest_settled,
    };
    Ok(ConsumerProgress {
        watermark,
        highest_settled,
        revision,
        next_retry_at_ms: next_retry,
    })
}
fn checkpoint(
    conn: &Connection,
    consumer: &str,
    partition: Option<&str>,
) -> Result<i64, StorageError> {
    match partition {
        Some(p) => Ok(progress_row(conn, consumer, p)?.0),
        None => Ok(progress(conn, consumer)?.watermark),
    }
}
fn awaiting_eligibility(
    conn: &Connection,
    consumer: &str,
    after: Option<&Id>,
    max: u16,
) -> Result<Vec<QueueEntry>, StorageError> {
    let n = limit(max)?;
    let mut s = stmt(
        conn,
        &format!(
            "SELECT {QUEUE_COLUMNS} FROM queue_entries WHERE consumer=?1 AND awaiting_eligibility=1 AND state='Reconciling' AND (?2 IS NULL OR id>?2) ORDER BY id LIMIT ?3"
        ),
    )?;
    let raws: Vec<QueueRaw> =
        db(db(s.query_map(params![consumer, after.map(Id::as_str), n], queue_row))?.collect())?;
    raws.into_iter().map(queue_of).collect()
}
fn next_due(conn: &Connection, now: i64) -> Result<Option<i64>, StorageError> {
    db(stmt(
        conn,
        "SELECT min(t) FROM (SELECT min(ready_at) AS t FROM queue_entries WHERE state='Ready' AND ready_at>?1 UNION ALL SELECT min(lease_until) FROM queue_entries WHERE state='Claimed')",
    )?
    .query_row([now], |r| r.get(0)))
}

fn package_state(conn: &Connection, package: &str) -> Result<Option<PackageState>, StorageError> {
    let raw: Option<(String, i64, i64, String)> = db(stmt(
        conn,
        "SELECT package_id,generation,revision,state_json FROM package_handler_state WHERE package_id=?1",
    )?
    .query_row([package], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
    })
    .optional())?;
    raw.map(|(package_id, generation, revision, state)| {
        Ok(PackageState {
            package_id,
            generation,
            revision,
            state: json_of(&state)?,
        })
    })
    .transpose()
}
fn processing(
    conn: &Connection,
    package: &str,
    subscription: &Id,
    event: &EventId,
) -> Result<Option<ProcessingRecord>, StorageError> {
    type Raw = (String, String, i64, i64, i64, String);
    let raw: Option<Raw> = db(stmt(
        conn,
        "SELECT receipt_id,payload_digest,state_generation,state_revision,checkpoint,intents_json FROM package_handler_commits WHERE package_id=?1 AND subscription_id=?2 AND event_id=?3",
    )?
    .query_row(
        params![package, subscription.as_str(), event.0.as_str()],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )
    .optional())?;
    raw.map(
        |(receipt, digest, generation, revision, checkpoint, intents)| {
            Ok(ProcessingRecord {
                package_id: package.to_owned(),
                subscription_id: subscription.clone(),
                event_id: event.clone(),
                receipt_id: id_of(receipt)?,
                payload_digest: digest,
                state_generation: generation,
                state_revision: revision,
                checkpoint,
                intents: json_of(&intents)?,
            })
        },
    )
    .transpose()
}

const GRANT_COLUMNS: &str =
    "id,actor_id,root_id,revision,state,authority_source,binding_json,expires_at,uses_remaining";
fn grant(conn: &Connection, id: &Id) -> Result<Option<GrantRecord>, StorageError> {
    type Raw = (
        String,
        String,
        String,
        i64,
        String,
        String,
        String,
        Option<i64>,
        Option<i64>,
    );
    let raw: Option<Raw> = db(stmt(
        conn,
        &format!("SELECT {GRANT_COLUMNS} FROM resume_grants WHERE id=?1"),
    )?
    .query_row([id.as_str()], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
            r.get(7)?,
            r.get(8)?,
        ))
    })
    .optional())?;
    raw.map(
        |(id, actor, root, revision, state, source, binding, expires, uses)| {
            Ok(GrantRecord {
                id: id_of(id)?,
                actor: ActorId(id_of(actor)?),
                root_id: RootId(id_of(root)?),
                revision,
                active: state == "Active",
                revoked: state == "Revoked",
                authority_source: source,
                binding: json_of(&binding)?,
                expires_at_ms: expires,
                uses_remaining: uses,
            })
        },
    )
    .transpose()
}

macro_rules! read_impls {
    ($t:ty) => {
        impl ActorRead for $t {
            fn actor_kind(&self, actor: &ActorId) -> Result<Option<PrincipalKind>, StorageError> {
                actor_kind(self.conn, actor)
            }
        }
        impl CommandLedgerRead for $t {
            fn lookup(
                &self,
                identity: &ReceiptIdentity,
            ) -> Result<Option<LedgerEntry>, StorageError> {
                lookup_receipt(self.conn, identity)
            }
            fn reply(
                &self,
                identity: &ReceiptIdentity,
            ) -> Result<Option<ReceiptReply>, StorageError> {
                receipt_reply(self.conn, identity)
            }
        }
        impl HistoryRead for $t {
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
        impl OperationRead for $t {
            fn operation(&self, id: &OperationId) -> Result<Option<OperationRecord>, StorageError> {
                operation(self.conn, id)
            }
            fn unresolved(&self, limit: u16) -> Result<Vec<OperationRecord>, StorageError> {
                unresolved(self.conn, limit)
            }
        }
        impl QueueRead for $t {
            fn entry(&self, id: &Id) -> Result<Option<QueueEntry>, StorageError> {
                entry(self.conn, id)
            }
            fn delivery(
                &self,
                consumer: &str,
                event: &EventId,
            ) -> Result<Option<QueueEntry>, StorageError> {
                delivery(self.conn, consumer, event)
            }
            fn status(&self, consumer: &str) -> Result<QueueStatus, StorageError> {
                status(self.conn, consumer)
            }
            fn deliveries(
                &self,
                filter: &DeliveryFilter,
                after: Option<&DeliveryKey>,
                limit: u16,
            ) -> Result<DeliveryPage, StorageError> {
                deliveries(self.conn, filter, after, limit)
            }
            fn progress(&self, consumer: &str) -> Result<ConsumerProgress, StorageError> {
                progress(self.conn, consumer)
            }
            fn checkpoint(
                &self,
                consumer: &str,
                partition: Option<&str>,
            ) -> Result<i64, StorageError> {
                checkpoint(self.conn, consumer, partition)
            }
            fn next_due_ms(&self, now_ms: i64) -> Result<Option<i64>, StorageError> {
                next_due(self.conn, now_ms)
            }
            fn awaiting_eligibility(
                &self,
                consumer: &str,
                after: Option<&Id>,
                limit: u16,
            ) -> Result<Vec<QueueEntry>, StorageError> {
                awaiting_eligibility(self.conn, consumer, after, limit)
            }
        }
        impl PackageHandlerRead for $t {
            fn package_state(
                &self,
                package_id: &str,
            ) -> Result<Option<PackageState>, StorageError> {
                package_state(self.conn, package_id)
            }
            fn processing(
                &self,
                package_id: &str,
                subscription_id: &Id,
                event_id: &EventId,
            ) -> Result<Option<ProcessingRecord>, StorageError> {
                processing(self.conn, package_id, subscription_id, event_id)
            }
        }
        impl GrantRead for $t {
            fn grant(&self, id: &Id) -> Result<Option<GrantRecord>, StorageError> {
                grant(self.conn, id)
            }
        }
    };
}

/// One read snapshot on a query-only connection.
pub(crate) struct Snapshot<'c> {
    pub conn: &'c Connection,
    pub stamp: SnapshotStamp,
}
read_impls!(Snapshot<'_>);
impl ReadSnapshot for Snapshot<'_> {
    fn stamp(&self) -> &SnapshotStamp {
        &self.stamp
    }
    fn actors(&self) -> &dyn ActorRead {
        self
    }
    fn receipts(&self) -> &dyn CommandLedgerRead {
        self
    }
    fn history(&self) -> &dyn HistoryRead {
        self
    }
    fn operations(&self) -> &dyn OperationRead {
        self
    }
    fn queue(&self) -> &dyn QueueRead {
        self
    }
    fn grants(&self) -> &dyn GrantRead {
        self
    }
    fn packages(&self) -> &dyn PackageHandlerRead {
        self
    }
    fn configuration(&self) -> &dyn ConfigurationRead {
        self
    }
    fn authority(&self) -> &dyn AuthorityRead {
        self
    }
    fn artifacts(&self) -> &dyn ArtifactRead {
        self
    }
    fn sessions(&self) -> &dyn SessionRead {
        self
    }
    fn resources(&self) -> &dyn ResourceRead {
        self
    }
    fn runs(&self) -> &dyn RunRead {
        self
    }
    fn diagnostics(&self) -> &dyn DiagnosticRead {
        self
    }
    fn router(&self) -> &dyn RouterRead {
        self
    }
    fn projects(&self) -> &dyn ProjectRead {
        self
    }
    fn work(&self) -> &dyn WorkRead {
        self
    }
}

// ---------------------------------------------------------------------------------------------
// The write unit

/// The writer's open `BEGIN IMMEDIATE` transaction with transaction-scoped repositories.
pub(crate) struct Unit<'c> {
    pub conn: &'c Connection,
    pub now: i64,
    pub ids: &'c dyn IdSource,
    pub info: &'c StoreInfo,
    /// Open savepoint depth.
    pub savepoints: u32,
    /// Serial of this unit of work; a savepoint issued by another unit is refused.
    pub serial: u64,
}
read_impls!(Unit<'_>);

impl Unit<'_> {
    fn open_savepoint(&self, sp: &Savepoint) -> Result<u32, StorageError> {
        if sp.transaction() != self.serial || sp.depth() == 0 || sp.depth() > self.savepoints {
            return Err(invalid("savepoint is not open in this transaction"));
        }
        Ok(sp.depth())
    }
    fn boot(&self) -> Result<&Id, StorageError> {
        self.info
            .boot_id
            .as_ref()
            .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))
    }
    /// Store-owned bookkeeping event attributed to the host actor.
    fn observe_queue(
        &mut self,
        entry: &QueueEntry,
        kind: &str,
        payload: Value,
    ) -> Result<i64, StorageError> {
        let id = EventId(self.ids.next_id(IdKind::Event));
        self.append(NewEvent {
            id,
            actor: self.info.host_actor.clone(),
            project_id: None,
            root_id: entry.root_id.clone(),
            operation_id: entry.operation_id.clone(),
            aggregate_kind: "queue_entry".into(),
            aggregate_id: entry.id.as_str().to_owned(),
            aggregate_revision: entry.revision.max(1),
            kind: kind.into(),
            payload,
        })
    }
    fn fenced(&self, id: &Id, fence: &QueueFence) -> Result<QueueEntry, StorageError> {
        let e = entry(self.conn, id)?.ok_or_else(|| StorageError::NotFound {
            kind: "queue_entry",
            id: id.as_str().to_owned(),
        })?;
        if e.state != QueueState::Claimed
            || e.owner.as_ref() != Some(&fence.owner)
            || e.generation != fence.generation
            || e.claim_boot_id.as_ref() != self.info.boot_id.as_ref()
        {
            return Err(StorageError::LeaseLost {
                kind: "queue_entry",
                id: id.as_str().to_owned(),
            });
        }
        Ok(e)
    }
    fn reload(&self, id: &Id) -> Result<QueueEntry, StorageError> {
        entry(self.conn, id)?.ok_or_else(|| corrupt("queue entry vanished inside its transaction"))
    }
    fn load_op(&self, id: &OperationId) -> Result<OperationRecord, StorageError> {
        operation(self.conn, id)?.ok_or_else(|| StorageError::NotFound {
            kind: "operation",
            id: id.0.as_str().to_owned(),
        })
    }
    fn op_cas(&self, op: &OperationRecord, expected: i64) -> Result<(), StorageError> {
        if op.revision != expected {
            return Err(StorageError::conflict(
                RevisionDimension::Operation,
                "operation",
                op.id.0.clone(),
                Revision::new(expected)
                    .map_err(|_| invalid("expected revision must be positive"))?,
                revision(op.revision)?,
            ));
        }
        Ok(())
    }
    fn set_op_state(&self, op: &OperationRecord, to: OperationState) -> Result<(), StorageError> {
        let n = db(stmt(
            self.conn,
            "UPDATE operations SET state=?1,revision=revision+1 WHERE id=?2 AND revision=?3",
        )?
        .execute(params![state_name(to), op.id.0.as_str(), op.revision]))?;
        if n != 1 {
            return Err(corrupt("operation changed inside its transaction"));
        }
        Ok(())
    }
    fn set_attempt(
        &self,
        attempt: &AttemptRecord,
        op: &OperationId,
        to: OperationState,
        observation: Option<&Value>,
    ) -> Result<(), StorageError> {
        db(stmt(
            self.conn,
            "UPDATE operation_attempts SET state=?1 WHERE operation_id=?2 AND attempt_no=?3",
        )?
        .execute(params![state_name(to), op.0.as_str(), attempt.attempt_no]))?;
        if let Some(o) = observation {
            self.push_observation(op, attempt.attempt_no, o)?;
        }
        Ok(())
    }
    fn push_observation(
        &self,
        op: &OperationId,
        attempt_no: i64,
        observation: &Value,
    ) -> Result<(), StorageError> {
        object("observation", observation)?;
        db(stmt(self.conn,
            "UPDATE operation_attempts SET observations_json=json_insert(observations_json,'$[#]',json(?1)) WHERE operation_id=?2 AND attempt_no=?3",
        )?
        .execute(params![text(observation), op.0.as_str(), attempt_no]))?;
        Ok(())
    }
}

impl WriteTransaction for Unit<'_> {
    fn now_ms(&self) -> i64 {
        self.now
    }
    fn new_id(&mut self, kind: IdKind) -> Id {
        self.ids.next_id(kind)
    }
    fn store(&self) -> &StoreInfo {
        self.info
    }
    fn committed_sequence(&self) -> Result<i64, StorageError> {
        committed_sequence(self.conn)
    }
    fn savepoint(&mut self) -> Result<Savepoint, StorageError> {
        let depth = self.savepoints + 1;
        db(self.conn.execute_batch(&format!("SAVEPOINT dw_sp{depth}")))?;
        self.savepoints = depth;
        Ok(Savepoint::issue(self.serial, depth))
    }
    fn rollback_to(&mut self, sp: Savepoint) -> Result<(), StorageError> {
        let depth = self.open_savepoint(&sp)?;
        // SQLite semantics: unwinding an outer savepoint also discards every inner one.
        db(self
            .conn
            .execute_batch(&format!("ROLLBACK TO dw_sp{depth}; RELEASE dw_sp{depth}")))?;
        self.savepoints = depth - 1;
        Ok(())
    }
    fn release(&mut self, sp: Savepoint) -> Result<(), StorageError> {
        let depth = self.open_savepoint(&sp)?;
        db(self.conn.execute_batch(&format!("RELEASE dw_sp{depth}")))?;
        self.savepoints = depth - 1;
        Ok(())
    }
    fn actors(&mut self) -> &mut dyn ActorRegistry {
        self
    }
    fn receipts(&mut self) -> &mut dyn CommandLedger {
        self
    }
    fn history(&mut self) -> &mut dyn HistoryLog {
        self
    }
    fn operations(&mut self) -> &mut dyn OperationJournal {
        self
    }
    fn queue(&mut self) -> &mut dyn DurableQueue {
        self
    }
    fn grants(&mut self) -> &mut dyn GrantLedger {
        self
    }
    fn packages(&mut self) -> &mut dyn PackageHandlerLedger {
        self
    }
    fn configuration(&mut self) -> &mut dyn ConfigurationRepository {
        self
    }
    fn authority(&mut self) -> &mut dyn AuthorityLedger {
        self
    }
    fn artifacts(&mut self) -> &mut dyn ArtifactRepository {
        self
    }
    fn sessions(&mut self) -> &mut dyn SessionRepository {
        self
    }
    fn resources(&mut self) -> &mut dyn ResourceRepository {
        self
    }
    fn runs(&mut self) -> &mut dyn RunRepository {
        self
    }
    fn diagnostics(&mut self) -> &mut dyn DiagnosticRepository {
        self
    }
    fn router(&mut self) -> &mut dyn RouterRepository {
        self
    }
    fn projects(&mut self) -> &mut dyn ProjectRepository {
        self
    }
    fn work(&mut self) -> &mut dyn WorkRepository {
        self
    }
}

impl ActorRegistry for Unit<'_> {
    fn register(
        &mut self,
        actor: &ActorId,
        kind: PrincipalKind,
        display_name: &str,
    ) -> Result<(), StorageError> {
        bounded("display name", display_name, 256)?;
        match actor_kind(self.conn, actor)? {
            Some(k) if k == kind => Ok(()),
            Some(_) => Err(StorageError::Duplicate {
                kind: "actor",
                id: actor.0.as_str().to_owned(),
            }),
            None => {
                db(stmt(
                    self.conn,
                    "INSERT INTO actors(id,kind,display_name) VALUES(?1,?2,?3)",
                )?
                .execute(params![
                    actor.0.as_str(),
                    principal_name(kind),
                    display_name
                ]))?;
                Ok(())
            }
        }
    }
}

impl CommandLedger for Unit<'_> {
    fn record(
        &mut self,
        identity: &ReceiptIdentity,
        receipt: &StoredReceipt,
    ) -> Result<(), StorageError> {
        if lookup_receipt(self.conn, identity)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "command_receipt",
                id: identity.request_key().0.as_str().to_owned(),
            });
        }
        let op = receipt.receipt.operation_id.as_ref().map(|o| o.0.as_str());
        require(self.conn, "operations", "operation", op)?;
        let body = json!({
            "contract": receipt.contract,
            "schemaRevision": receipt.schema_revision.0,
            "receipt": receipt.receipt,
        });
        db(stmt(self.conn,
            "INSERT INTO command_receipts(actor_id,scope_key,idempotency_key,command_kind,payload_digest,operation_id,result_json,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        )?
        .execute(params![
            identity.actor_id().0.as_str(),
            identity.scope_key().as_str(),
            identity.request_key().0.as_str(),
            receipt.operation,
            receipt.fingerprint.as_str(),
            op,
            text(&body),
            self.now
        ]))?;
        Ok(())
    }

    fn record_reply(
        &mut self,
        identity: &ReceiptIdentity,
        reply: &ReceiptReply,
    ) -> Result<(), StorageError> {
        object("reply schema", &reply.schema)?;
        if lookup_receipt(self.conn, identity)?.is_none() {
            return Err(StorageError::NotFound {
                kind: "command_receipt",
                id: identity.request_key().0.as_str().to_owned(),
            });
        }
        let duplicate = receipt_reply(self.conn, identity)?.is_some()
            || db(stmt(
                self.conn,
                "SELECT 1 FROM receipt_replies WHERE command_id=?1 OR artifact_id=?2",
            )?
            .exists(params![
                reply.command_id.0.as_str(),
                reply.artifact_id.as_str()
            ]))?;
        if duplicate {
            return Err(StorageError::Duplicate {
                kind: "receipt_reply",
                id: reply.command_id.0.as_str().to_owned(),
            });
        }
        let belongs = db(stmt(
            self.conn,
            "SELECT 1 FROM artifact_versions WHERE id=?1 AND artifact_id=?2",
        )?
        .exists(params![
            reply.version_id.as_str(),
            reply.artifact_id.as_str()
        ]))?;
        if !belongs {
            return Err(StorageError::NotFound {
                kind: "artifact_version",
                id: reply.version_id.as_str().to_owned(),
            });
        }
        db(stmt(self.conn,
            "INSERT INTO receipt_replies(actor_id,scope_key,idempotency_key,command_id,artifact_id,version_id,schema_json,published_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        )?
        .execute(params![
            identity.actor_id().0.as_str(),
            identity.scope_key().as_str(),
            identity.request_key().0.as_str(),
            reply.command_id.0.as_str(),
            reply.artifact_id.as_str(),
            reply.version_id.as_str(),
            text(&reply.schema),
            reply.published_at_ms
        ]))?;
        Ok(())
    }
}

impl HistoryLog for Unit<'_> {
    fn append(&mut self, e: NewEvent) -> Result<i64, StorageError> {
        bounded("aggregate kind", &e.aggregate_kind, 128)?;
        bounded("aggregate id", &e.aggregate_id, 256)?;
        bounded("event kind", &e.kind, 128)?;
        object("event payload", &e.payload)?;
        if e.aggregate_revision <= 0 {
            return Err(invalid("aggregate revision must be positive"));
        }
        if actor_kind(self.conn, &e.actor)?.is_none() {
            return Err(StorageError::NotFound {
                kind: "actor",
                id: e.actor.0.as_str().to_owned(),
            });
        }
        require(
            self.conn,
            "projects",
            "project",
            e.project_id.as_ref().map(|p| p.0.as_str()),
        )?;
        require(
            self.conn,
            "work_items",
            "root",
            e.root_id.as_ref().map(|r| r.0.as_str()),
        )?;
        require(
            self.conn,
            "operations",
            "operation",
            e.operation_id.as_ref().map(|o| o.0.as_str()),
        )?;
        if exists(
            self.conn,
            "SELECT 1 FROM events WHERE id=?1",
            e.id.0.as_str(),
        )? {
            return Err(StorageError::Duplicate {
                kind: "event",
                id: e.id.0.as_str().to_owned(),
            });
        }
        db(stmt(self.conn,
            "INSERT INTO events(id,actor_id,project_id,root_id,operation_id,aggregate_kind,aggregate_id,aggregate_revision,kind,created_at,payload_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
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
            self.now,
            // History is persisted and exported: credential-shaped content in free text is
            // redacted before it is written.
            text(&devworks_core::authority::redact::Redactor::new().redact_json(&e.payload))
        ]))?;
        Ok(self.conn.last_insert_rowid())
    }
}

impl OperationJournal for Unit<'_> {
    fn record_intent(&mut self, op: NewOperation) -> Result<OperationRecord, StorageError> {
        bounded("capability", &op.capability, 256)?;
        bounded("args digest", &op.args_digest, 128)?;
        object("operation payload", &op.payload)?;
        if op.schema_version <= 0 {
            return Err(invalid("schema version must be positive"));
        }
        require(
            self.conn,
            "work_items",
            "root",
            op.root_id.as_ref().map(|r| r.0.as_str()),
        )?;
        require(
            self.conn,
            "assignments",
            "assignment",
            op.assignment_id.as_ref().map(|a| a.0.as_str()),
        )?;
        if operation(self.conn, &op.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "operation",
                id: op.id.0.as_str().to_owned(),
            });
        }
        db(stmt(self.conn,
            "INSERT INTO operations(id,root_id,assignment_id,capability,schema_version,args_digest,payload_json,state,revision,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,'IntentRecorded',1,?8)",
        )?
        .execute(params![
            op.id.0.as_str(),
            op.root_id.as_ref().map(|r| r.0.as_str()),
            op.assignment_id.as_ref().map(|a| a.0.as_str()),
            op.capability,
            op.schema_version,
            op.args_digest,
            text(&op.payload),
            self.now
        ]))?;
        self.load_op(&op.id)
    }

    fn begin_attempt(
        &mut self,
        id: &OperationId,
        expected_revision: i64,
        binding: AttemptBinding,
    ) -> Result<AttemptRecord, StorageError> {
        let op = self.load_op(id)?;
        self.op_cas(&op, expected_revision)?;
        if !matches!(
            op.state,
            OperationState::IntentRecorded | OperationState::RetryPending
        ) {
            return Err(invalid(format!(
                "cannot start an attempt from {}",
                state_name(op.state)
            )));
        }
        require(
            self.conn,
            "executions",
            "execution",
            binding.execution_id.as_ref().map(|e| e.0.as_str()),
        )?;
        let boot = self.boot()?.clone();
        let attempt_no = op.attempts.last().map_or(1, |a| a.attempt_no + 1);
        let attempt_id = self.ids.next_id(IdKind::Attempt);
        db(stmt(self.conn,
            "INSERT INTO operation_attempts(id,operation_id,attempt_no,boot_id,execution_id,owner_generation,control_revision,policy_revision,state,observations_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'Preflighted','[]')",
        )?
        .execute(params![
            attempt_id.as_str(),
            id.0.as_str(),
            attempt_no,
            boot.as_str(),
            binding.execution_id.as_ref().map(|e| e.0.as_str()),
            binding.owner_generation,
            binding.control_revision,
            binding.policy_revision
        ]))?;
        self.set_op_state(&op, OperationState::Preflighted)?;
        self.load_op(id)?
            .attempts
            .pop()
            .ok_or_else(|| corrupt("attempt vanished inside its transaction"))
    }

    fn transition(
        &mut self,
        id: &OperationId,
        expected_revision: i64,
        fence: Option<&AttemptFence>,
        to: OperationState,
        observation: Option<Value>,
    ) -> Result<OperationRecord, StorageError> {
        let op = self.load_op(id)?;
        self.op_cas(&op, expected_revision)?;
        if !transition_allowed(op.state, to) {
            return Err(invalid(format!(
                "illegal operation transition {} -> {}",
                state_name(op.state),
                state_name(to)
            )));
        }
        let attempt_bound = matches!(
            op.state,
            OperationState::Preflighted
                | OperationState::Dispatching
                | OperationState::OutcomeUnknown
                | OperationState::NeedsInput
        );
        let latest = op.latest_attempt();
        if matches!(
            op.state,
            OperationState::Preflighted | OperationState::Dispatching
        ) {
            let current = latest.map(AttemptRecord::fence);
            if fence.is_none() || fence != current.as_ref() {
                return Err(StorageError::LeaseLost {
                    kind: "operation_attempt",
                    id: id.0.as_str().to_owned(),
                });
            }
        }
        self.set_op_state(&op, to)?;
        match (attempt_bound, latest) {
            (true, Some(attempt)) => self.set_attempt(attempt, id, to, observation.as_ref())?,
            _ if observation.is_some() => {
                return Err(invalid(
                    "no current attempt to attribute the observation to",
                ));
            }
            _ => {}
        }
        self.load_op(id)
    }

    fn observe(
        &mut self,
        id: &OperationId,
        attempt_no: i64,
        observation: Value,
    ) -> Result<(), StorageError> {
        let op = self.load_op(id)?;
        if !op.attempts.iter().any(|a| a.attempt_no == attempt_no) {
            return Err(StorageError::NotFound {
                kind: "operation_attempt",
                id: format!("{}#{attempt_no}", id.0.as_str()),
            });
        }
        self.push_observation(id, attempt_no, &observation)
    }

    fn classify_interrupted(
        &mut self,
        max: u16,
    ) -> Result<Vec<(OperationId, OperationState)>, StorageError> {
        let n = limit(max)?;
        let boot = self.boot()?.clone();
        let ids: Vec<String> = {
            let mut s = stmt(
                self.conn,
                "SELECT o.id FROM operations o JOIN operation_attempts a ON a.operation_id=o.id AND a.attempt_no=(SELECT max(attempt_no) FROM operation_attempts WHERE operation_id=o.id) WHERE o.state IN ('Preflighted','Dispatching') AND a.boot_id<>?1 ORDER BY o.id LIMIT ?2",
            )?;
            db(db(s.query_map(params![boot.as_str(), n], |r| r.get(0)))?.collect())?
        };
        let mut out = Vec::new();
        for raw in ids {
            let id = OperationId(id_of(raw)?);
            let op = self.load_op(&id)?;
            let to = match op.state {
                OperationState::Preflighted => OperationState::RetryPending,
                _ => OperationState::OutcomeUnknown,
            };
            let attempt = op
                .latest_attempt()
                .ok_or_else(|| corrupt("interrupted operation without attempt"))?;
            self.set_op_state(&op, to)?;
            let note = json!({"recovery": state_name(to), "observedInBoot": boot.as_str(), "attemptBoot": attempt.boot_id.as_str()});
            self.set_attempt(attempt, &id, to, Some(&note))?;
            out.push((id, to));
        }
        Ok(out)
    }
}

impl Unit<'_> {
    fn queue_entry(&self, id: &Id) -> Result<QueueEntry, StorageError> {
        entry(self.conn, id)?.ok_or_else(|| StorageError::NotFound {
            kind: "queue_entry",
            id: id.as_str().to_owned(),
        })
    }
    /// Records a settlement in the consumer's progress rows: the reserved `''` row counts
    /// settlements and keeps the highest settled sequence; an ordered partition's row is its
    /// contiguous checkpoint. A delivery behind the partition's head can be settled first (an
    /// operator cancels a queued delivery), so the checkpoint is recomputed as the settled prefix:
    /// the highest settled event sequence below the lowest unsettled delivery of the partition
    /// (or the highest settled one when nothing is unsettled). It is always the sequence of a
    /// settled delivery of this partition, never a sequence in between, so a checkpoint an
    /// in-flight invocation read stays valid while deliveries behind it settle.
    fn settled(&self, e: &QueueEntry) -> Result<(), StorageError> {
        db(stmt(self.conn,
            "INSERT INTO consumer_progress(consumer,partition_key,applied_seq,revision) VALUES(?1,'',?2,1) ON CONFLICT(consumer,partition_key) DO UPDATE SET applied_seq=max(applied_seq,excluded.applied_seq),revision=revision+1",
        )?
        .execute(params![e.consumer, e.event_sequence]))?;
        if let Some(p) = e.partition.as_deref() {
            let prefix: Option<i64> = db(stmt(
                self.conn,
                &format!(
                    "SELECT max(event_seq) FROM queue_entries WHERE consumer=?1 AND partition_key=?2 AND state IN {SETTLED} AND event_seq<coalesce((SELECT min(event_seq) FROM queue_entries WHERE consumer=?1 AND partition_key=?2 AND state NOT IN {SETTLED}),9223372036854775807)"
                ),
            )?
            .query_row(params![e.consumer, p], |r| r.get(0)))?;
            db(stmt(self.conn,
                "INSERT INTO consumer_progress(consumer,partition_key,applied_seq,revision) VALUES(?1,?2,?3,1) ON CONFLICT(consumer,partition_key) DO UPDATE SET applied_seq=max(applied_seq,excluded.applied_seq),revision=revision+1",
            )?
            .execute(params![e.consumer, p, prefix.unwrap_or(0)]))?;
        }
        Ok(())
    }
    /// Cancels the delivery's operation before dispatch. Refused when the effect may have started
    /// or already has an outcome; an operation already cancelled before dispatch is left as is.
    fn cancel_operation(&mut self, e: &QueueEntry) -> Result<(), StorageError> {
        let Some(op_id) = &e.operation_id else {
            return Ok(());
        };
        let op = self.load_op(op_id)?;
        if op.state == OperationState::CancelledBeforeDispatch {
            return Ok(());
        }
        if !cancellable_before_dispatch(op.state) {
            return Err(invalid(format!(
                "the delivery's operation is {}; only an undispatched operation can be cancelled",
                state_name(op.state)
            )));
        }
        // A preflighted attempt is closed under its own fence, with the reason as its
        // observation; an operation without a live attempt has nothing to attribute it to.
        let fence = match op.state {
            OperationState::Preflighted => op.latest_attempt().map(AttemptRecord::fence),
            _ => None,
        };
        let note = fence
            .as_ref()
            .map(|_| json!({"cancelledWithDelivery": e.id.as_str()}));
        OperationJournal::transition(
            self,
            op_id,
            op.revision,
            fence.as_ref(),
            OperationState::CancelledBeforeDispatch,
            note,
        )?;
        Ok(())
    }
    /// Whether a requested cancellation applies instead of making `e` ready again: there is no
    /// operation, or it has certainly not dispatched. An established outcome (`ObservedComplete`,
    /// `Failed`) must still be recorded by redelivery, so the request cannot apply to it.
    fn cancel_applies(&self, e: &QueueEntry) -> Result<bool, StorageError> {
        Ok(e.cancel_requested && cancellation_applies(self, e)?)
    }
    /// Settles an unclaimed or claimed delivery `Cancelled` with its undispatched operation.
    fn settle_cancelled(
        &mut self,
        e: &QueueEntry,
        cause: &str,
    ) -> Result<QueueEntry, StorageError> {
        self.cancel_operation(e)?;
        db(stmt(self.conn,
            "UPDATE queue_entries SET state='Cancelled',owner_id=NULL,lease_until=NULL,claim_boot_id=NULL,awaiting_eligibility=0,revision=revision+1 WHERE id=?1 AND generation=?2",
        )?
        .execute(params![e.id.as_str(), e.generation]))?;
        let after = self.reload(&e.id)?;
        self.settled(&after)?;
        self.observe_queue(&after, "QueueDeliveryCancelled", json!({"cause": cause}))?;
        Ok(after)
    }
    fn not_possibly_dispatched(&self, e: &QueueEntry, action: &str) -> Result<(), StorageError> {
        if let Some(op) = &e.operation_id
            && possibly_dispatched(self.load_op(op)?.state)
        {
            return Err(invalid(format!(
                "the delivery's operation outcome is unresolved; reconcile it before {action}"
            )));
        }
        Ok(())
    }
}

impl DurableQueue for Unit<'_> {
    fn enqueue(&mut self, e: NewQueueEntry) -> Result<QueueEntry, StorageError> {
        bounded("consumer", &e.consumer, 128)?;
        object("queue payload", &e.payload)?;
        if let Some(p) = &e.partition {
            bounded("partition", p, 256)?;
        }
        if e.ready_at_ms < 0 {
            return Err(invalid("ready time must be non-negative"));
        }
        let seq: i64 = db(stmt(self.conn, "SELECT seq FROM events WHERE id=?1")?
            .query_row([e.event_id.0.as_str()], |r| r.get(0))
            .optional())?
        .ok_or_else(|| StorageError::NotFound {
            kind: "event",
            id: e.event_id.0.as_str().to_owned(),
        })?;
        require(
            self.conn,
            "work_items",
            "root",
            e.root_id.as_ref().map(|r| r.0.as_str()),
        )?;
        require(
            self.conn,
            "operations",
            "operation",
            e.operation_id.as_ref().map(|o| o.0.as_str()),
        )?;
        let duplicate: bool = db(stmt(
            self.conn,
            "SELECT 1 FROM queue_entries WHERE consumer=?1 AND event_id=?2",
        )?
        .exists(params![e.consumer, e.event_id.0.as_str()]))?;
        if duplicate
            || exists(
                self.conn,
                "SELECT 1 FROM queue_entries WHERE id=?1",
                e.id.as_str(),
            )?
        {
            return Err(StorageError::Duplicate {
                kind: "queue_delivery",
                id: format!("{}/{}", e.consumer, e.event_id.0.as_str()),
            });
        }
        if let Some(p) = &e.partition {
            // An ordered partition is appended in event order: a delivery for an older event
            // than one already enqueued or applied there would be delivered out of order.
            let later: bool = db(stmt(
                self.conn,
                "SELECT 1 FROM queue_entries WHERE consumer=?1 AND partition_key=?2 AND event_seq>?3",
            )?
            .exists(params![e.consumer, p, seq]))?;
            if later || progress_row(self.conn, &e.consumer, p)?.0 >= seq {
                return Err(invalid(
                    "ordered partition deliveries must be enqueued in event order",
                ));
            }
        }
        db(stmt(self.conn,
            "INSERT INTO queue_entries(id,consumer,event_id,root_id,operation_id,priority,ready_at,state,payload_json,partition_key,event_seq) VALUES(?1,?2,?3,?4,?5,?6,?7,'Ready',?8,?9,?10)",
        )?
        .execute(params![
            e.id.as_str(),
            e.consumer,
            e.event_id.0.as_str(),
            e.root_id.as_ref().map(|r| r.0.as_str()),
            e.operation_id.as_ref().map(|o| o.0.as_str()),
            e.priority,
            e.ready_at_ms,
            text(&e.payload),
            e.partition,
            seq
        ]))?;
        self.reload(&e.id)
    }

    fn claim_ready(&mut self, req: &ClaimRequest) -> Result<Vec<QueueEntry>, StorageError> {
        let n = limit(req.limit)?;
        bounded("consumer", &req.consumer, 128)?;
        if req.lease_ms <= 0 {
            return Err(invalid("lease must be positive"));
        }
        let lease_until = self
            .now
            .checked_add(req.lease_ms)
            .ok_or_else(|| invalid("lease overflows"))?;
        // Eligible: ready now; the head of its ordered partition (no older unsettled delivery);
        // and its operation is not possibly dispatched (uncertain effects pause dependents).
        let candidates: Vec<(String, i64)> = {
            let mut s = stmt(
                self.conn,
                &format!(
                    "SELECT q.id,q.generation FROM queue_entries q WHERE q.consumer=?1 AND q.state='Ready' AND q.ready_at<=?2 \
                     AND (q.partition_key IS NULL OR NOT EXISTS (SELECT 1 FROM queue_entries p WHERE p.consumer=q.consumer AND p.partition_key=q.partition_key AND p.event_seq<q.event_seq AND p.state NOT IN {SETTLED})) \
                     AND (q.operation_id IS NULL OR NOT EXISTS (SELECT 1 FROM operations o WHERE o.id=q.operation_id AND o.state IN ('Dispatching','OutcomeUnknown','NeedsInput'))) \
                     ORDER BY q.priority DESC,q.ready_at,q.id LIMIT ?3"
                ),
            )?;
            db(db(s.query_map(params![req.consumer, self.now, n], |r| {
                Ok((r.get(0)?, r.get(1)?))
            }))?
            .collect())?
        };
        let boot = self.boot()?.clone();
        let mut claimed = Vec::new();
        for (id, generation) in candidates {
            let changed = db(stmt(self.conn,
                "UPDATE queue_entries SET state='Claimed',owner_id=?1,lease_until=?2,generation=generation+1,attempt_count=attempt_count+1,revision=revision+1,claim_boot_id=?5 WHERE id=?3 AND state='Ready' AND generation=?4",
            )?
            .execute(params![req.owner.as_str(), lease_until, id, generation, boot.as_str()]))?;
            if changed != 1 {
                continue; // lost race or no longer eligible
            }
            let e = self.reload(&id_of(id)?)?;
            self.observe_queue(
                &e,
                "QueueClaimed",
                json!({"owner": req.owner.as_str(), "generation": e.generation, "leaseUntil": lease_until, "attempt": e.attempt_count, "boot": boot.as_str()}),
            )?;
            claimed.push(e);
        }
        Ok(claimed)
    }

    fn renew(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        lease_ms: i64,
    ) -> Result<QueueEntry, StorageError> {
        let e = self.fenced(id, fence)?;
        if lease_ms <= 0 {
            return Err(invalid("lease must be positive"));
        }
        if e.lease_until_ms.is_some_and(|l| l < self.now) {
            // An expired lease is not renewable; recovery decides what happens next.
            return Err(StorageError::LeaseLost {
                kind: "queue_entry",
                id: id.as_str().to_owned(),
            });
        }
        let until = self
            .now
            .checked_add(lease_ms)
            .ok_or_else(|| invalid("lease overflows"))?;
        db(stmt(
            self.conn,
            "UPDATE queue_entries SET lease_until=?1,revision=revision+1 WHERE id=?2 AND generation=?3",
        )?
        .execute(params![until, id.as_str(), fence.generation]))?;
        self.reload(id)
    }

    fn settle(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        settlement: Settlement,
    ) -> Result<QueueEntry, StorageError> {
        let e = self.fenced(id, fence)?;
        let state = match settlement {
            Settlement::Completed => QueueState::Completed,
            Settlement::Cancelled => {
                self.cancel_operation(&e)?;
                QueueState::Cancelled
            }
        };
        // A completion clears the visible failure; a cancellation keeps it as evidence.
        db(stmt(self.conn,
            "UPDATE queue_entries SET state=?1,owner_id=NULL,lease_until=NULL,claim_boot_id=NULL,revision=revision+1,last_error=CASE WHEN ?1='Completed' THEN NULL ELSE last_error END WHERE id=?2 AND generation=?3",
        )?
        .execute(params![queue_state_name(state), id.as_str(), fence.generation]))?;
        let after = self.reload(id)?;
        self.settled(&after)?;
        Ok(after)
    }

    fn fail(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        cause: &str,
        disposition: FailureDisposition,
    ) -> Result<QueueEntry, StorageError> {
        bounded("failure cause", cause, 1024)?;
        let e = self.fenced(id, fence)?;
        let dispatched = match &e.operation_id {
            Some(op) => possibly_dispatched(self.load_op(op)?.state),
            None => false,
        };
        let (state, ready_at) = match disposition {
            FailureDisposition::Retry {
                delay_ms,
                max_attempts,
            } => {
                if delay_ms < 0 || max_attempts <= 0 {
                    return Err(invalid("retry delay/attempt bounds"));
                }
                if dispatched {
                    // A retry hint never authorizes replaying an effect that may have happened,
                    // and an exhausted budget never turns it into poison: hold the delivery for
                    // reconciliation, exactly as claim recovery does.
                    (QueueState::Reconciling, e.ready_at_ms)
                } else if self.cancel_applies(&e)? {
                    // A cancellation requested during the attempt wins over its retry.
                    return self.settle_cancelled(&e, cause);
                } else if e.attempt_count >= max_attempts {
                    (QueueState::Failed, e.ready_at_ms)
                } else {
                    (
                        QueueState::Ready,
                        self.now
                            .checked_add(delay_ms)
                            .ok_or_else(|| invalid("retry delay overflows"))?,
                    )
                }
            }
            // A possibly dispatched effect is held, never poison.
            FailureDisposition::Terminal if dispatched => (QueueState::Reconciling, e.ready_at_ms),
            FailureDisposition::Terminal => (QueueState::Failed, e.ready_at_ms),
        };
        let failed_at = (state != QueueState::Ready).then_some(self.now);
        db(stmt(self.conn,
            "UPDATE queue_entries SET state=?1,ready_at=?2,owner_id=NULL,lease_until=NULL,claim_boot_id=NULL,revision=revision+1,last_error=?5,failed_at=coalesce(?6,failed_at) WHERE id=?3 AND generation=?4",
        )?
        .execute(params![queue_state_name(state), ready_at, id.as_str(), fence.generation, cause, failed_at]))?;
        let after = self.reload(id)?;
        self.observe_queue(
            &after,
            "QueueAttemptFailed",
            json!({
                "cause": cause,
                "attempt": after.attempt_count,
                "outcome": queue_state_name(state),
                "readyAt": ready_at,
                "terminal": state == QueueState::Failed,
                "held": state == QueueState::Reconciling,
            }),
        )?;
        Ok(after)
    }

    fn cancel(&mut self, id: &Id) -> Result<QueueEntry, StorageError> {
        let e = self.queue_entry(id)?;
        let op_state = match &e.operation_id {
            Some(op) => Some(self.load_op(op)?.state),
            None => None,
        };
        let request = |me: &mut Self| -> Result<QueueEntry, StorageError> {
            db(stmt(
                me.conn,
                "UPDATE queue_entries SET cancellation_epoch=cancellation_epoch+1,cancel_requested=1,revision=revision+1 WHERE id=?1",
            )?
            .execute([id.as_str()]))?;
            let after = me.reload(id)?;
            me.observe_queue(
                &after,
                "QueueCancellationRequested",
                json!({"state": queue_state_name(after.state)}),
            )?;
            Ok(after)
        };
        match e.state {
            // The effect may have happened: record the request; reconciliation decides.
            QueueState::Reconciling if op_state.is_some_and(possibly_dispatched) => request(self),
            QueueState::Ready | QueueState::Reconciling => {
                // The same rule as the dispatcher: an operation already cancelled before
                // dispatch is an idempotent no-op, an established outcome is refused.
                if !cancellation_applies(self, &e)? {
                    return Err(invalid(
                        "the delivery's operation already has an outcome; it cannot be cancelled",
                    ));
                }
                db(stmt(
                    self.conn,
                    "UPDATE queue_entries SET cancellation_epoch=cancellation_epoch+1,revision=revision+1 WHERE id=?1",
                )?
                .execute([id.as_str()]))?;
                let e = self.reload(id)?;
                self.settle_cancelled(&e, "cancelled before dispatch")
            }
            QueueState::Claimed => request(self),
            QueueState::Cancelled => Ok(e),
            QueueState::Completed | QueueState::Failed | QueueState::Dismissed => {
                Err(invalid("a settled delivery cannot be cancelled"))
            }
        }
    }

    fn recover_claims(
        &mut self,
        mode: ClaimRecovery,
        max: u16,
    ) -> Result<RecoveryOutcome, StorageError> {
        let n = limit(max)?;
        let stale: Vec<String> = {
            let sql = match mode {
                ClaimRecovery::Expired => {
                    "SELECT id FROM queue_entries WHERE state='Claimed' AND lease_until<?1 ORDER BY lease_until,id LIMIT ?2"
                }
                ClaimRecovery::PriorBoots => {
                    "SELECT id FROM queue_entries WHERE state='Claimed' AND (claim_boot_id IS NULL OR claim_boot_id<>?1) ORDER BY lease_until,id LIMIT ?2"
                }
            };
            let mut s = stmt(self.conn, sql)?;
            match mode {
                ClaimRecovery::Expired => {
                    db(db(s.query_map(params![self.now, n], |r| r.get(0)))?.collect())?
                }
                ClaimRecovery::PriorBoots => {
                    let boot = self.boot()?.clone();
                    db(db(s.query_map(params![boot.as_str(), n], |r| r.get(0)))?.collect())?
                }
            }
        };
        let mut out = RecoveryOutcome::default();
        for raw in stale {
            let id = id_of(raw)?;
            let e = self.reload(&id)?;
            let dispatched = match &e.operation_id {
                Some(op) => possibly_dispatched(self.load_op(op)?.state),
                // Handler database effects commit atomically with completion, so redelivering an
                // entry without an external operation is safe (at-least-once).
                None => false,
            };
            if !dispatched && self.cancel_applies(&e)? {
                self.settle_cancelled(&e, "cancellation requested before claim recovery")?;
                out.cancelled.push(id);
                continue;
            }
            // Root-scoped work of a dead process is not continued automatically: the root may
            // have been paused or held meanwhile, so it waits until its eligibility is confirmed.
            let awaiting = mode == ClaimRecovery::PriorBoots && e.root_id.is_some();
            let state = if dispatched || awaiting {
                QueueState::Reconciling
            } else {
                QueueState::Ready
            };
            let failed_at = (state == QueueState::Reconciling).then_some(self.now);
            db(stmt(self.conn,
                "UPDATE queue_entries SET state=?1,owner_id=NULL,lease_until=NULL,claim_boot_id=NULL,generation=generation+1,revision=revision+1,awaiting_eligibility=?4,failed_at=coalesce(?5,failed_at) WHERE id=?2 AND state='Claimed' AND generation=?3",
            )?
            .execute(params![queue_state_name(state), id.as_str(), e.generation, awaiting, failed_at]))?;
            let after = self.reload(&id)?;
            self.observe_queue(
                &after,
                "QueueClaimRecovered",
                json!({
                    "previousOwner": e.owner.as_ref().map(Id::as_str),
                    "previousGeneration": e.generation,
                    "outcome": queue_state_name(state),
                    "mode": format!("{mode:?}"),
                    "awaitingEligibility": awaiting,
                }),
            )?;
            if state == QueueState::Reconciling {
                out.held.push(id);
            } else {
                out.requeued.push(id);
            }
        }
        Ok(out)
    }

    fn requeue(&mut self, id: &Id, delay_ms: i64) -> Result<QueueEntry, StorageError> {
        let e = self.queue_entry(id)?;
        if !matches!(e.state, QueueState::Reconciling | QueueState::Failed) {
            return Err(invalid("only held or failed deliveries can be requeued"));
        }
        if delay_ms < 0 {
            return Err(invalid("delay must be non-negative"));
        }
        self.not_possibly_dispatched(&e, "a retry")?;
        if self.cancel_applies(&e)? {
            return self.settle_cancelled(&e, "cancellation requested before the retry");
        }
        let ready = self
            .now
            .checked_add(delay_ms)
            .ok_or_else(|| invalid("delay overflows"))?;
        // From Failed the retry budget restarts; the failure cause stays visible until the
        // delivery completes.
        let attempts = if e.state == QueueState::Failed {
            0
        } else {
            e.attempt_count
        };
        db(stmt(self.conn,
            "UPDATE queue_entries SET state='Ready',ready_at=?1,attempt_count=?4,generation=generation+1,revision=revision+1,awaiting_eligibility=0 WHERE id=?2 AND generation=?3",
        )?
        .execute(params![ready, id.as_str(), e.generation, attempts]))?;
        let after = self.reload(id)?;
        self.observe_queue(
            &after,
            "QueueRequeued",
            json!({"readyAt": ready, "from": queue_state_name(e.state), "previousAttempts": e.attempt_count}),
        )?;
        Ok(after)
    }

    fn hold(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        cause: &str,
    ) -> Result<QueueEntry, StorageError> {
        bounded("hold cause", cause, 1024)?;
        self.fenced(id, fence)?;
        db(stmt(self.conn,
            "UPDATE queue_entries SET state='Reconciling',owner_id=NULL,lease_until=NULL,claim_boot_id=NULL,generation=generation+1,revision=revision+1,last_error=?1,failed_at=?4 WHERE id=?2 AND generation=?3",
        )?
        .execute(params![cause, id.as_str(), fence.generation, self.now]))?;
        let after = self.reload(id)?;
        self.observe_queue(&after, "QueueDeliveryHeld", json!({"cause": cause}))?;
        Ok(after)
    }

    fn hold_for_eligibility(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        cause: &str,
    ) -> Result<QueueEntry, StorageError> {
        bounded("hold cause", cause, 1024)?;
        let e = self.fenced(id, fence)?;
        if e.root_id.is_none() {
            return Err(invalid(
                "only root-scoped deliveries wait for root eligibility",
            ));
        }
        db(stmt(self.conn,
            "UPDATE queue_entries SET state='Reconciling',owner_id=NULL,lease_until=NULL,claim_boot_id=NULL,generation=generation+1,revision=revision+1,last_error=?1,failed_at=?4,awaiting_eligibility=1 WHERE id=?2 AND generation=?3",
        )?
        .execute(params![cause, id.as_str(), fence.generation, self.now]))?;
        let after = self.reload(id)?;
        self.observe_queue(
            &after,
            "QueueDeliveryAwaitingEligibility",
            json!({"cause": cause}),
        )?;
        Ok(after)
    }

    fn dismiss(&mut self, id: &Id, reason: &str) -> Result<QueueEntry, StorageError> {
        if reason.len() > 65_536 || reason.contains('\0') {
            return Err(invalid("dismissal reason bounds"));
        }
        let e = self.queue_entry(id)?;
        if e.state != QueueState::Failed {
            return Err(invalid("only failed deliveries can be dismissed"));
        }
        self.not_possibly_dispatched(&e, "dismissal")?;
        db(stmt(self.conn,
            "UPDATE queue_entries SET state='Dismissed',revision=revision+1 WHERE id=?1 AND state='Failed' AND generation=?2",
        )?
        .execute(params![id.as_str(), e.generation]))?;
        let after = self.reload(id)?;
        self.settled(&after)?;
        self.observe_queue(
            &after,
            "QueueDeliveryDismissed",
            json!({"reason": reason, "cause": after.last_error}),
        )?;
        Ok(after)
    }

    fn reassert(
        &mut self,
        id: &Id,
        fence: &QueueFence,
        lease_ms: i64,
    ) -> Result<QueueEntry, StorageError> {
        let e = self.fenced(id, fence)?;
        if lease_ms <= 0 {
            return Err(invalid("lease must be positive"));
        }
        let until = self
            .now
            .checked_add(lease_ms)
            .ok_or_else(|| invalid("lease overflows"))?;
        db(stmt(
            self.conn,
            "UPDATE queue_entries SET lease_until=?1,revision=revision+1 WHERE id=?2 AND generation=?3",
        )?
        .execute(params![until, id.as_str(), fence.generation]))?;
        let after = self.reload(id)?;
        if e.lease_until_ms.is_some_and(|l| l < self.now) {
            self.observe_queue(
                &after,
                "QueueLeaseReasserted",
                json!({"owner": fence.owner.as_str(), "expiredAt": e.lease_until_ms, "leaseUntil": until}),
            )?;
        }
        Ok(after)
    }

    fn compact(&mut self, id: &Id) -> Result<(), StorageError> {
        let e = self.queue_entry(id)?;
        if !e.state.is_settled() {
            return Err(invalid(
                "only completed, cancelled or dismissed deliveries can be compacted",
            ));
        }
        // Tombstone: the row (and its UNIQUE(consumer, event_id)) stays; its bookkeeping goes.
        db(stmt(self.conn,
            "UPDATE queue_entries SET state='Compacted',payload_json='{}',owner_id=NULL,lease_until=NULL,claim_boot_id=NULL WHERE id=?1",
        )?
        .execute([id.as_str()]))?;
        Ok(())
    }
}

impl PackageHandlerLedger for Unit<'_> {
    fn compare_and_set_state(
        &mut self,
        package_id: &str,
        generation: i64,
        expected_revision: i64,
        state: Value,
    ) -> Result<PackageState, StorageError> {
        bounded("package id", package_id, 256)?;
        if generation <= 0 || expected_revision < 0 {
            return Err(invalid("package state generation/revision bounds"));
        }
        let current = package_state(self.conn, package_id)?;
        if let Some(c) = &current
            && c.generation != generation
        {
            return Err(StorageError::LeaseLost {
                kind: "package_state",
                id: package_id.to_owned(),
            });
        }
        let current_revision = current.as_ref().map_or(0, |c| c.revision);
        if current_revision != expected_revision {
            return Err(StorageError::conflict(
                RevisionDimension::Registry,
                "package_state",
                Id::new(sha256_id(package_id)).map_err(|_| invalid("package id"))?,
                Revision::new(expected_revision.max(1))
                    .map_err(|_| invalid("expected revision"))?,
                Revision::new(current_revision.max(1)).map_err(|_| invalid("current revision"))?,
            ));
        }
        db(stmt(self.conn,
            "INSERT INTO package_handler_state(package_id,generation,revision,state_json) VALUES(?1,?2,1,?3) ON CONFLICT(package_id) DO UPDATE SET revision=revision+1,state_json=excluded.state_json WHERE revision=?4 AND generation=?2",
        )?
        .execute(params![package_id, generation, text(&state), expected_revision]))?;
        package_state(self.conn, package_id)?
            .ok_or_else(|| corrupt("package state vanished inside its transaction"))
    }

    fn record_processing(&mut self, r: &ProcessingRecord) -> Result<(), StorageError> {
        bounded("package id", &r.package_id, 256)?;
        bounded("payload digest", &r.payload_digest, 128)?;
        require(self.conn, "events", "event", Some(r.event_id.0.as_str()))?;
        let duplicate = processing(self.conn, &r.package_id, &r.subscription_id, &r.event_id)?
            .is_some()
            || exists(
                self.conn,
                "SELECT 1 FROM package_handler_commits WHERE receipt_id=?1",
                r.receipt_id.as_str(),
            )?;
        if duplicate {
            return Err(StorageError::Duplicate {
                kind: "package_processing",
                id: format!(
                    "{}/{}/{}",
                    r.package_id,
                    r.subscription_id.as_str(),
                    r.event_id.0.as_str()
                ),
            });
        }
        db(stmt(self.conn,
            "INSERT INTO package_handler_commits(package_id,subscription_id,event_id,receipt_id,payload_digest,state_generation,state_revision,checkpoint,intents_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        )?
        .execute(params![
            r.package_id,
            r.subscription_id.as_str(),
            r.event_id.0.as_str(),
            r.receipt_id.as_str(),
            r.payload_digest,
            r.state_generation,
            r.state_revision,
            r.checkpoint,
            text(&r.intents)
        ]))?;
        Ok(())
    }
}

/// Opaque conflict target for a package (package IDs are qualified names, not opaque IDs).
fn sha256_id(package_id: &str) -> String {
    devworks_core::fingerprint::sha256_hex(package_id.as_bytes())
}

impl GrantLedger for Unit<'_> {
    fn issue(&mut self, g: NewGrant) -> Result<GrantRecord, StorageError> {
        bounded("authority source", &g.authority_source, 256)?;
        object("grant binding", &g.binding)?;
        if g.uses.is_some_and(|u| u <= 0) {
            return Err(invalid("grant uses must be positive"));
        }
        if actor_kind(self.conn, &g.actor)?.is_none() {
            return Err(StorageError::NotFound {
                kind: "actor",
                id: g.actor.0.as_str().to_owned(),
            });
        }
        require(self.conn, "work_items", "root", Some(g.root_id.0.as_str()))?;
        if grant(self.conn, &g.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "grant",
                id: g.id.as_str().to_owned(),
            });
        }
        db(stmt(self.conn,
            "INSERT INTO resume_grants(id,actor_id,root_id,revision,state,authority_source,binding_json,expires_at,uses_remaining) VALUES(?1,?2,?3,1,'Active',?4,?5,?6,?7)",
        )?
        .execute(params![
            g.id.as_str(),
            g.actor.0.as_str(),
            g.root_id.0.as_str(),
            g.authority_source,
            text(&g.binding),
            g.expires_at_ms,
            g.uses
        ]))?;
        grant(self.conn, &g.id)?.ok_or_else(|| corrupt("grant vanished inside its transaction"))
    }

    fn consume(&mut self, use_: &GrantUse, actor: &ActorId) -> Result<GrantRecord, StorageError> {
        let g = grant(self.conn, &use_.grant_id)?.ok_or_else(|| StorageError::NotFound {
            kind: "grant",
            id: use_.grant_id.as_str().to_owned(),
        })?;
        if &g.actor != actor
            || !g.active
            || g.expires_at_ms.is_some_and(|t| t <= self.now)
            || g.uses_remaining.is_some_and(|u| u <= 0)
        {
            return Err(StorageError::ScopeDenied);
        }
        if g.revision != use_.expected_revision {
            return Err(StorageError::conflict(
                RevisionDimension::Authority,
                "grant",
                g.id.clone(),
                Revision::new(use_.expected_revision)
                    .map_err(|_| invalid("expected revision must be positive"))?,
                revision(g.revision)?,
            ));
        }
        let remaining = g.uses_remaining.map(|u| u - 1);
        let state = if remaining == Some(0) {
            "Consumed"
        } else {
            "Active"
        };
        let n = db(stmt(self.conn,
            "UPDATE resume_grants SET uses_remaining=?1,state=?2,revision=revision+1 WHERE id=?3 AND revision=?4",
        )?
        .execute(params![remaining, state, g.id.as_str(), g.revision]))?;
        if n != 1 {
            return Err(corrupt("grant changed inside its transaction"));
        }
        grant(self.conn, &g.id)?.ok_or_else(|| corrupt("grant vanished inside its transaction"))
    }

    fn revoke(&mut self, id: &Id, expected_revision: i64) -> Result<GrantRecord, StorageError> {
        let g = grant(self.conn, id)?.ok_or_else(|| StorageError::NotFound {
            kind: "grant",
            id: id.as_str().to_owned(),
        })?;
        if g.revision != expected_revision {
            return Err(StorageError::conflict(
                RevisionDimension::Authority,
                "grant",
                g.id.clone(),
                Revision::new(expected_revision)
                    .map_err(|_| invalid("expected revision must be positive"))?,
                revision(g.revision)?,
            ));
        }
        let n = db(stmt(
            self.conn,
            "UPDATE resume_grants SET state='Revoked',revision=revision+1 WHERE id=?1 AND revision=?2",
        )?
        .execute(params![g.id.as_str(), g.revision]))?;
        if n != 1 {
            return Err(corrupt("grant changed inside its transaction"));
        }
        grant(self.conn, &g.id)?.ok_or_else(|| corrupt("grant vanished inside its transaction"))
    }
}
