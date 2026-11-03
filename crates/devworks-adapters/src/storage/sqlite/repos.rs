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
    ActorId, AssignmentId, AttemptId, CONTRACT, EventId, ExecutionId, Id, OperationId,
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
        other => return Err(corrupt(format!("unknown queue state {other}"))),
    })
}
pub(crate) fn principal_name(k: PrincipalKind) -> &'static str {
    match k {
        PrincipalKind::Human => "human",
        PrincipalKind::Agent => "agent",
        PrincipalKind::Package => "package",
        PrincipalKind::Schedule => "schedule",
        PrincipalKind::Host => "host",
    }
}
fn parse_principal(s: &str) -> Result<PrincipalKind, StorageError> {
    Ok(match s {
        "human" => PrincipalKind::Human,
        "agent" => PrincipalKind::Agent,
        "package" => PrincipalKind::Package,
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

fn actor_kind(conn: &Connection, actor: &ActorId) -> Result<Option<PrincipalKind>, StorageError> {
    let kind: Option<String> = db(stmt(conn, "SELECT kind FROM actors WHERE id=?1")?
        .query_row([actor.0.as_str()], |r| r.get(0))
        .optional())?;
    kind.as_deref().map(parse_principal).transpose()
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

const EVENT_COLUMNS: &str = "seq,id,actor_id,project_id,root_id,operation_id,aggregate_kind,aggregate_id,aggregate_revision,kind,created_at,payload_json";
fn event_row(r: &Row<'_>) -> rusqlite::Result<[Option<String>; 12]> {
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
fn event_of(raw: [Option<String>; 12]) -> Result<EventRecord, StorageError> {
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

const QUEUE_COLUMNS: &str = "id,consumer,event_id,root_id,operation_id,priority,ready_at,state,owner_id,lease_until,generation,cancellation_epoch,attempt_count,payload_json,claim_boot_id";
type QueueRaw = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
    String,
    Option<String>,
    Option<i64>,
    i64,
    i64,
    i64,
    String,
    Option<String>,
);
fn queue_row(r: &Row<'_>) -> rusqlite::Result<QueueRaw> {
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
        r.get(10)?,
        r.get(11)?,
        r.get(12)?,
        r.get(13)?,
        r.get(14)?,
    ))
}
fn queue_of(raw: QueueRaw) -> Result<QueueEntry, StorageError> {
    let (
        id,
        consumer,
        event,
        root,
        op,
        priority,
        ready,
        state,
        owner,
        lease,
        gen_,
        epoch,
        attempts,
        payload,
        claim_boot,
    ) = raw;
    Ok(QueueEntry {
        id: id_of(id)?,
        consumer,
        event_id: EventId(id_of(event)?),
        root_id: opt_id(root)?.map(RootId),
        operation_id: opt_id(op)?.map(OperationId),
        priority,
        ready_at_ms: ready,
        state: parse_queue_state(&state)?,
        owner: opt_id(owner)?,
        lease_until_ms: lease,
        generation: gen_,
        cancellation_epoch: epoch,
        attempt_count: attempts,
        payload: json_of(&payload)?,
        claim_boot_id: opt_id(claim_boot)?,
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
        }
    }
    Ok(out)
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
            fn status(&self, consumer: &str) -> Result<QueueStatus, StorageError> {
                status(self.conn, consumer)
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
    fn configuration(&self) -> &dyn ConfigurationRead {
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
            aggregate_revision: entry.generation.max(1),
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
    fn configuration(&mut self) -> &mut dyn ConfigurationRepository {
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
            text(&e.payload)
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

impl DurableQueue for Unit<'_> {
    fn enqueue(&mut self, e: NewQueueEntry) -> Result<QueueEntry, StorageError> {
        bounded("consumer", &e.consumer, 128)?;
        object("queue payload", &e.payload)?;
        if e.ready_at_ms < 0 {
            return Err(invalid("ready time must be non-negative"));
        }
        require(self.conn, "events", "event", Some(e.event_id.0.as_str()))?;
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
        db(stmt(self.conn,
            "INSERT INTO queue_entries(id,consumer,event_id,root_id,operation_id,priority,ready_at,state,payload_json) VALUES(?1,?2,?3,?4,?5,?6,?7,'Ready',?8)",
        )?
        .execute(params![
            e.id.as_str(),
            e.consumer,
            e.event_id.0.as_str(),
            e.root_id.as_ref().map(|r| r.0.as_str()),
            e.operation_id.as_ref().map(|o| o.0.as_str()),
            e.priority,
            e.ready_at_ms,
            text(&e.payload)
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
        let candidates: Vec<(String, i64)> = {
            let mut s = stmt(
                self.conn,
                "SELECT id,generation FROM queue_entries WHERE consumer=?1 AND state='Ready' AND ready_at<=?2 ORDER BY priority DESC,ready_at,id LIMIT ?3",
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
                "UPDATE queue_entries SET state='Claimed',owner_id=?1,lease_until=?2,generation=generation+1,attempt_count=attempt_count+1,claim_boot_id=?5 WHERE id=?3 AND state='Ready' AND generation=?4",
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
            "UPDATE queue_entries SET lease_until=?1 WHERE id=?2 AND generation=?3",
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
        self.fenced(id, fence)?;
        let state = match settlement {
            Settlement::Completed => QueueState::Completed,
            Settlement::Cancelled => QueueState::Cancelled,
        };
        db(stmt(self.conn,
            "UPDATE queue_entries SET state=?1,owner_id=NULL,lease_until=NULL,claim_boot_id=NULL WHERE id=?2 AND generation=?3",
        )?
        .execute(params![queue_state_name(state), id.as_str(), fence.generation]))?;
        self.reload(id)
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
        let (state, ready_at) = match disposition {
            FailureDisposition::Retry {
                delay_ms,
                max_attempts,
            } => {
                if delay_ms < 0 || max_attempts <= 0 {
                    return Err(invalid("retry delay/attempt bounds"));
                }
                let dispatched = match &e.operation_id {
                    Some(op) => possibly_dispatched(self.load_op(op)?.state),
                    None => false,
                };
                if e.attempt_count >= max_attempts {
                    (QueueState::Failed, e.ready_at_ms)
                } else if dispatched {
                    // A retry hint never authorizes replaying an effect that may have happened:
                    // hold the delivery for reconciliation, exactly as claim recovery does.
                    (QueueState::Reconciling, e.ready_at_ms)
                } else {
                    (
                        QueueState::Ready,
                        self.now
                            .checked_add(delay_ms)
                            .ok_or_else(|| invalid("retry delay overflows"))?,
                    )
                }
            }
            FailureDisposition::Terminal => (QueueState::Failed, e.ready_at_ms),
        };
        db(stmt(self.conn,
            "UPDATE queue_entries SET state=?1,ready_at=?2,owner_id=NULL,lease_until=NULL,claim_boot_id=NULL WHERE id=?3 AND generation=?4",
        )?
        .execute(params![queue_state_name(state), ready_at, id.as_str(), fence.generation]))?;
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
        let e = entry(self.conn, id)?.ok_or_else(|| StorageError::NotFound {
            kind: "queue_entry",
            id: id.as_str().to_owned(),
        })?;
        match e.state {
            QueueState::Ready | QueueState::Reconciling => {
                db(stmt(self.conn,
                    "UPDATE queue_entries SET state='Cancelled',cancellation_epoch=cancellation_epoch+1 WHERE id=?1",
                )?
                .execute([id.as_str()]))?;
            }
            QueueState::Claimed => {
                db(stmt(
                    self.conn,
                    "UPDATE queue_entries SET cancellation_epoch=cancellation_epoch+1 WHERE id=?1",
                )?
                .execute([id.as_str()]))?;
            }
            QueueState::Cancelled => return Ok(e),
            QueueState::Completed | QueueState::Failed => {
                return Err(invalid("a settled delivery cannot be cancelled"));
            }
        }
        self.reload(id)
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
            let held = match &e.operation_id {
                Some(op) => possibly_dispatched(self.load_op(op)?.state),
                // Handler database effects commit atomically with completion, so redelivering an
                // entry without an external operation is safe (at-least-once).
                None => false,
            };
            let state = if held {
                QueueState::Reconciling
            } else {
                QueueState::Ready
            };
            db(stmt(self.conn,
                "UPDATE queue_entries SET state=?1,owner_id=NULL,lease_until=NULL,claim_boot_id=NULL,generation=generation+1 WHERE id=?2 AND state='Claimed' AND generation=?3",
            )?
            .execute(params![queue_state_name(state), id.as_str(), e.generation]))?;
            let after = self.reload(&id)?;
            self.observe_queue(
                &after,
                "QueueClaimRecovered",
                json!({
                    "previousOwner": e.owner.as_ref().map(Id::as_str),
                    "previousGeneration": e.generation,
                    "outcome": queue_state_name(state),
                    "mode": format!("{mode:?}"),
                }),
            )?;
            if held {
                out.held.push(id);
            } else {
                out.requeued.push(id);
            }
        }
        Ok(out)
    }

    fn requeue(&mut self, id: &Id, delay_ms: i64) -> Result<QueueEntry, StorageError> {
        let e = entry(self.conn, id)?.ok_or_else(|| StorageError::NotFound {
            kind: "queue_entry",
            id: id.as_str().to_owned(),
        })?;
        if !matches!(e.state, QueueState::Reconciling | QueueState::Failed) {
            return Err(invalid("only held or failed deliveries can be requeued"));
        }
        if delay_ms < 0 {
            return Err(invalid("delay must be non-negative"));
        }
        if let Some(op) = &e.operation_id
            && possibly_dispatched(self.load_op(op)?.state)
        {
            return Err(invalid(
                "the delivery's operation outcome is unresolved; reconcile it first",
            ));
        }
        let ready = self
            .now
            .checked_add(delay_ms)
            .ok_or_else(|| invalid("delay overflows"))?;
        let attempts = if e.state == QueueState::Failed {
            0
        } else {
            e.attempt_count
        };
        db(stmt(self.conn,
            "UPDATE queue_entries SET state='Ready',ready_at=?1,attempt_count=?4,generation=generation+1 WHERE id=?2 AND generation=?3",
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

    fn compact(&mut self, id: &Id) -> Result<(), StorageError> {
        let e = entry(self.conn, id)?.ok_or_else(|| StorageError::NotFound {
            kind: "queue_entry",
            id: id.as_str().to_owned(),
        })?;
        if !matches!(e.state, QueueState::Completed | QueueState::Cancelled) {
            return Err(invalid(
                "only completed or cancelled deliveries can be compacted",
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
}
