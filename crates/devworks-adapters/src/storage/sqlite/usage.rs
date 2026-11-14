//! Canonical attempt accounting. SQL sums canonical balances; legacy source facts remain separate.
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{Id, Revision, RevisionDimension};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, Row, params, params_from_iter};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value};
fn invalid(s: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(s.into())
}
fn corrupt(s: impl Into<String>) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, s)
}
fn conflict(key: &str, e: i64, c: i64) -> StorageError {
    StorageError::conflict(
        RevisionDimension::Configuration,
        "usage",
        Id::new(key).unwrap_or_else(|_| Id::new("usage").expect("literal")),
        Revision::new(e.max(1)).expect("positive"),
        Revision::new(c.max(1)).expect("positive"),
    )
}
fn field<T: DeserializeOwned>(v: &Value, k: &str) -> Result<T, StorageError> {
    serde_json::from_value(v[k].clone())
        .map_err(|_| corrupt(format!("invalid stored usage field {k}")))
}
fn doc(r: &Row<'_>) -> rusqlite::Result<Value> {
    let mut m = Map::new();
    for (i, k) in r.as_ref().column_names().enumerate() {
        let v = match r.get::<_, Sql>(i)? {
            Sql::Null => Value::Null,
            Sql::Integer(n) => Value::from(n),
            Sql::Text(s) => Value::String(s),
            _ => {
                return Err(rusqlite::Error::InvalidColumnType(
                    i,
                    k.into(),
                    r.get_ref(i)?.data_type(),
                ));
            }
        };
        m.insert(k.into(), v);
    }
    Ok(Value::Object(m))
}
fn rows<P: rusqlite::Params, T>(
    c: &Connection,
    q: &str,
    p: P,
    f: fn(&Value) -> Result<T, StorageError>,
) -> Result<Vec<T>, StorageError> {
    let mut s = db(c.prepare_cached(q))?;
    db(db(s.query_map(p, doc))?.collect::<rusqlite::Result<Vec<_>>>())?
        .iter()
        .map(f)
        .collect()
}
fn boolean(v: &Value, k: &str) -> Result<bool, StorageError> {
    match v[k].as_i64() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(corrupt("invalid stored usage boolean")),
    }
}
fn json(v: &Value, k: &str) -> Result<Value, StorageError> {
    serde_json::from_str(&field::<String>(v, k)?).map_err(|_| corrupt("invalid stored usage JSON"))
}
fn sql<T: Serialize>(v: &T) -> Result<Sql, StorageError> {
    match serde_json::to_value(v).map_err(|_| invalid("usage value serialization"))? {
        Value::Null => Ok(Sql::Null),
        Value::Bool(v) => Ok(Sql::Integer(i64::from(v))),
        Value::Number(n) => n
            .as_i64()
            .map(Sql::Integer)
            .ok_or_else(|| invalid("usage number outside exact integer range")),
        Value::String(s) => Ok(Sql::Text(s)),
        v => Ok(Sql::Text(v.to_string())),
    }
}
fn page(n: u16) -> Result<i64, StorageError> {
    if n == 0 || n > MAX_PAGE {
        Err(invalid("usage page limit must be 1..=MAX_PAGE"))
    } else {
        Ok(i64::from(n))
    }
}
fn bounded(s: &str, max: usize) -> Result<(), StorageError> {
    if s.is_empty() || s.chars().count() > max || s.contains('\0') {
        Err(invalid("usage text exceeds bound"))
    } else {
        Ok(())
    }
}
fn object(v: &Value) -> Result<(), StorageError> {
    if !v.is_object() || v.to_string().len() > 512 * 1024 {
        Err(invalid("usage object must fit 512KiB"))
    } else {
        Ok(())
    }
}
fn digest(s: &str) -> Result<(), StorageError> {
    if s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(invalid("usage digest must be lowercase SHA256"))
    }
}
/// Provider identities share the retained protocol Text contract, including Unicode/newlines.
fn provider_model_text(value: &str) -> Result<(), StorageError> {
    serde_json::from_value::<devworks_core::protocol::support::Text>(Value::String(value.into()))
        .map(|_| ())
        .map_err(|_| invalid("provider model identity is outside the Text contract"))
}
fn positive(n: i64) -> Result<(), StorageError> {
    if n > 0 {
        Ok(())
    } else {
        Err(invalid("usage revision must be positive"))
    }
}
fn nonnegative(n: Option<i64>) -> Result<(), StorageError> {
    if n.is_some_and(|v| v < 0) {
        Err(invalid("usage amount/time must be nonnegative"))
    } else {
        Ok(())
    }
}
fn classification(s: &str) -> Result<(), StorageError> {
    if [
        "ReportedCharge",
        "ConfiguredEstimate",
        "OfficialEstimate",
        "SubscriptionApiEquivalent",
        "LocalFree",
        "Untracked",
    ]
    .contains(&s)
    {
        Ok(())
    } else {
        Err(invalid("invalid usage classification"))
    }
}
fn cas(
    key: &str,
    current: Option<i64>,
    expected: Option<i64>,
    next: i64,
) -> Result<(), StorageError> {
    match (current, expected) {
        (None, None) if next == 1 => Ok(()),
        (Some(c), Some(e)) if c == e && e.checked_add(1) == Some(next) => Ok(()),
        (Some(c), Some(e)) => Err(conflict(key, e, c)),
        (None, Some(_)) => Err(StorageError::NotFound {
            kind: "usage",
            id: key.into(),
        }),
        (Some(_), None) => Err(StorageError::Duplicate {
            kind: "usage",
            id: key.into(),
        }),
        _ => Err(invalid("initial usage revision must be 1")),
    }
}
fn write(
    c: &Connection,
    table: &str,
    columns: &str,
    values: Vec<Sql>,
    update: bool,
    key_columns: usize,
) -> Result<(), StorageError> {
    let cols: Vec<_> = columns.split(',').collect();
    let placeholders = (1..=cols.len())
        .map(|n| format!("?{n}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut q = format!("INSERT INTO {table}({columns}) VALUES({placeholders})");
    if update {
        let keys = cols[..key_columns].join(",");
        let changes = cols[key_columns..]
            .iter()
            .map(|k| format!("{k}=excluded.{k}"))
            .collect::<Vec<_>>()
            .join(",");
        q.push_str(&format!(" ON CONFLICT({keys}) DO UPDATE SET {changes}"));
    }
    db(c.execute(&q, params_from_iter(values)))?;
    Ok(())
}
const ATTEMPT_COLS: &str = "attempt_id,request_id,operation_id,attempt_number,project_id,feature_id,work_id,assignment_id,execution_root_id,execution_id,session_id,job_run_id,actor_id,role,purpose,catalog_model_id,connection_id,endpoint_profile,access_mode,provider_model_id,pricing_snapshot_json,state,revision,created_at";
fn decode_attempt(v: &Value) -> Result<StoredInferenceAttempt, StorageError> {
    Ok(StoredInferenceAttempt {
        attempt_id: field(v, "attempt_id")?,
        request_id: field(v, "request_id")?,
        operation_id: field(v, "operation_id")?,
        attempt_number: field(v, "attempt_number")?,
        project_id: field(v, "project_id")?,
        feature_id: field(v, "feature_id")?,
        work_id: field(v, "work_id")?,
        assignment_id: field(v, "assignment_id")?,
        execution_root_id: field(v, "execution_root_id")?,
        execution_id: field(v, "execution_id")?,
        session_id: field(v, "session_id")?,
        job_run_id: field(v, "job_run_id")?,
        actor_id: field(v, "actor_id")?,
        role: field(v, "role")?,
        purpose: field(v, "purpose")?,
        catalog_model_id: field(v, "catalog_model_id")?,
        connection_id: field(v, "connection_id")?,
        endpoint_profile: field(v, "endpoint_profile")?,
        access_mode: field(v, "access_mode")?,
        provider_model_id: field(v, "provider_model_id")?,
        pricing_snapshot: json(v, "pricing_snapshot_json")?,
        state: field(v, "state")?,
        revision: field(v, "revision")?,
        created_at_ms: field(v, "created_at")?,
    })
}
fn write_attempt(
    c: &Connection,
    r: &StoredInferenceAttempt,
    update: bool,
) -> Result<(), StorageError> {
    write(
        c,
        "usage_attempts",
        ATTEMPT_COLS,
        vec![
            sql(&r.attempt_id)?,
            sql(&r.request_id)?,
            sql(&r.operation_id)?,
            sql(&r.attempt_number)?,
            sql(&r.project_id)?,
            sql(&r.feature_id)?,
            sql(&r.work_id)?,
            sql(&r.assignment_id)?,
            sql(&r.execution_root_id)?,
            sql(&r.execution_id)?,
            sql(&r.session_id)?,
            sql(&r.job_run_id)?,
            sql(&r.actor_id)?,
            sql(&r.role)?,
            sql(&r.purpose)?,
            sql(&r.catalog_model_id)?,
            sql(&r.connection_id)?,
            sql(&r.endpoint_profile)?,
            sql(&r.access_mode)?,
            sql(&r.provider_model_id)?,
            sql(&r.pricing_snapshot)?,
            sql(&r.state)?,
            sql(&r.revision)?,
            sql(&r.created_at_ms)?,
        ],
        update,
        1,
    )
}
const OBSERVATION_COLS: &str =
    "attempt_id,observation_key,sequence,digest,payload_json,recorded_at";
fn decode_observation(v: &Value) -> Result<StoredUsageObservation, StorageError> {
    Ok(StoredUsageObservation {
        attempt_id: field(v, "attempt_id")?,
        observation_key: field(v, "observation_key")?,
        sequence: field(v, "sequence")?,
        digest: field(v, "digest")?,
        payload: json(v, "payload_json")?,
        recorded_at_ms: field(v, "recorded_at")?,
    })
}
fn write_observation(
    c: &Connection,
    r: &StoredUsageObservation,
    update: bool,
) -> Result<(), StorageError> {
    write(
        c,
        "usage_observations",
        OBSERVATION_COLS,
        vec![
            sql(&r.attempt_id)?,
            sql(&r.observation_key)?,
            sql(&r.sequence)?,
            sql(&r.digest)?,
            sql(&r.payload)?,
            sql(&r.recorded_at_ms)?,
        ],
        update,
        2,
    )
}
const BALANCE_COLS: &str = "attempt_id,revision,priced_micro_usd,reserved_micro_usd,classification,monetary_complete,incomplete,unknown_tail,local_quiescent,summary_json";
fn decode_balance(v: &Value) -> Result<StoredAttemptBalance, StorageError> {
    Ok(StoredAttemptBalance {
        attempt_id: field(v, "attempt_id")?,
        revision: field(v, "revision")?,
        priced_micro_usd: field(v, "priced_micro_usd")?,
        reserved_micro_usd: field(v, "reserved_micro_usd")?,
        classification: field(v, "classification")?,
        monetary_complete: boolean(v, "monetary_complete")?,
        incomplete: boolean(v, "incomplete")?,
        unknown_tail: boolean(v, "unknown_tail")?,
        local_quiescent: boolean(v, "local_quiescent")?,
        summary: json(v, "summary_json")?,
    })
}
fn write_balance(
    c: &Connection,
    r: &StoredAttemptBalance,
    update: bool,
) -> Result<(), StorageError> {
    write(
        c,
        "usage_attempt_balances",
        BALANCE_COLS,
        vec![
            sql(&r.attempt_id)?,
            sql(&r.revision)?,
            sql(&r.priced_micro_usd)?,
            sql(&r.reserved_micro_usd)?,
            sql(&r.classification)?,
            sql(&r.monetary_complete)?,
            sql(&r.incomplete)?,
            sql(&r.unknown_tail)?,
            sql(&r.local_quiescent)?,
            sql(&r.summary)?,
        ],
        update,
        1,
    )
}
const BUDGET_COLS: &str = "feature_id,revision,limit_micro_usd,warning_revision";
fn decode_budget(v: &Value) -> Result<StoredFeatureBudget, StorageError> {
    Ok(StoredFeatureBudget {
        feature_id: field(v, "feature_id")?,
        revision: field(v, "revision")?,
        limit_micro_usd: field(v, "limit_micro_usd")?,
        warning_revision: field(v, "warning_revision")?,
    })
}
fn write_budget(c: &Connection, r: &StoredFeatureBudget, update: bool) -> Result<(), StorageError> {
    write(
        c,
        "usage_feature_budgets",
        BUDGET_COLS,
        vec![
            sql(&r.feature_id)?,
            sql(&r.revision)?,
            sql(&r.limit_micro_usd)?,
            sql(&r.warning_revision)?,
        ],
        update,
        1,
    )
}
const ORIGIN_COLS: &str = "source_kind,source_id,attempt_id,request_id,project_id,feature_id,work_id,session_id,job_run_id,payload_json,recorded_at,priced_micro_usd,classification,monetary_complete,incomplete,unknown_tail";
fn decode_origin(v: &Value) -> Result<StoredUsageOrigin, StorageError> {
    Ok(StoredUsageOrigin {
        source_kind: field(v, "source_kind")?,
        source_id: field(v, "source_id")?,
        attempt_id: field(v, "attempt_id")?,
        request_id: field(v, "request_id")?,
        project_id: field(v, "project_id")?,
        feature_id: field(v, "feature_id")?,
        work_id: field(v, "work_id")?,
        session_id: field(v, "session_id")?,
        job_run_id: field(v, "job_run_id")?,
        payload: json(v, "payload_json")?,
        recorded_at_ms: field(v, "recorded_at")?,
        priced_micro_usd: field(v, "priced_micro_usd")?,
        classification: field(v, "classification")?,
        monetary_complete: boolean(v, "monetary_complete")?,
        incomplete: boolean(v, "incomplete")?,
        unknown_tail: boolean(v, "unknown_tail")?,
    })
}
fn write_origin(c: &Connection, r: &StoredUsageOrigin, update: bool) -> Result<(), StorageError> {
    write(
        c,
        "usage_origins",
        ORIGIN_COLS,
        vec![
            sql(&r.source_kind)?,
            sql(&r.source_id)?,
            sql(&r.attempt_id)?,
            sql(&r.request_id)?,
            sql(&r.project_id)?,
            sql(&r.feature_id)?,
            sql(&r.work_id)?,
            sql(&r.session_id)?,
            sql(&r.job_run_id)?,
            sql(&r.payload)?,
            sql(&r.recorded_at_ms)?,
            sql(&r.priced_micro_usd)?,
            sql(&r.classification)?,
            sql(&r.monetary_complete)?,
            sql(&r.incomplete)?,
            sql(&r.unknown_tail)?,
        ],
        update,
        2,
    )
}
const CHANGE_COLS: &str =
    "event_id,attempt_id,feature_id,kind,delta_micro_usd,payload_json,created_at";
fn decode_change(v: &Value) -> Result<StoredUsageChange, StorageError> {
    Ok(StoredUsageChange {
        event_id: field(v, "event_id")?,
        attempt_id: field(v, "attempt_id")?,
        feature_id: field(v, "feature_id")?,
        kind: field(v, "kind")?,
        delta_micro_usd: field(v, "delta_micro_usd")?,
        payload: json(v, "payload_json")?,
        created_at_ms: field(v, "created_at")?,
    })
}
fn write_change(c: &Connection, r: &StoredUsageChange, update: bool) -> Result<(), StorageError> {
    write(
        c,
        "usage_changes",
        CHANGE_COLS,
        vec![
            sql(&r.event_id)?,
            sql(&r.attempt_id)?,
            sql(&r.feature_id)?,
            sql(&r.kind)?,
            sql(&r.delta_micro_usd)?,
            sql(&r.payload)?,
            sql(&r.created_at_ms)?,
        ],
        update,
        1,
    )
}
const SNAPSHOT_COLS: &str = "id,provider,revision,source_url,source_digest,parser_version,fetched_at,verified_at,effective_date,tariffs_json";
fn decode_snapshot(v: &Value) -> Result<StoredPriceSnapshot, StorageError> {
    Ok(StoredPriceSnapshot {
        id: field(v, "id")?,
        provider: field(v, "provider")?,
        revision: field(v, "revision")?,
        source_url: field(v, "source_url")?,
        source_digest: field(v, "source_digest")?,
        parser_version: field(v, "parser_version")?,
        fetched_at_ms: field(v, "fetched_at")?,
        verified_at_ms: field(v, "verified_at")?,
        effective_date: field(v, "effective_date")?,
        tariffs: serde_json::from_value(json(v, "tariffs_json")?)
            .map_err(|_| corrupt("invalid stored tariffs"))?,
    })
}
fn write_snapshot(
    c: &Connection,
    r: &StoredPriceSnapshot,
    update: bool,
) -> Result<(), StorageError> {
    write(
        c,
        "usage_price_snapshots",
        SNAPSHOT_COLS,
        vec![
            sql(&r.id)?,
            sql(&r.provider)?,
            sql(&r.revision)?,
            sql(&r.source_url)?,
            sql(&r.source_digest)?,
            sql(&r.parser_version)?,
            sql(&r.fetched_at_ms)?,
            sql(&r.verified_at_ms)?,
            sql(&r.effective_date)?,
            sql(&r.tariffs)?,
        ],
        update,
        1,
    )
}
const STATUS_COLS: &str = "provider,revision,current_snapshot_id,last_attempt,last_success,refresh_pending,last_error,next_refresh,refresh_operation_id";
fn decode_status(v: &Value) -> Result<StoredPriceStatus, StorageError> {
    Ok(StoredPriceStatus {
        provider: field(v, "provider")?,
        revision: field(v, "revision")?,
        current_snapshot_id: field(v, "current_snapshot_id")?,
        last_attempt_ms: field(v, "last_attempt")?,
        last_success_ms: field(v, "last_success")?,
        refresh_pending: boolean(v, "refresh_pending")?,
        refresh_operation_id: field(v, "refresh_operation_id")?,
        last_error: field(v, "last_error")?,
        next_refresh_ms: field(v, "next_refresh")?,
    })
}
fn write_status(c: &Connection, r: &StoredPriceStatus, update: bool) -> Result<(), StorageError> {
    write(
        c,
        "usage_price_status",
        STATUS_COLS,
        vec![
            sql(&r.provider)?,
            sql(&r.revision)?,
            sql(&r.current_snapshot_id)?,
            sql(&r.last_attempt_ms)?,
            sql(&r.last_success_ms)?,
            sql(&r.refresh_pending)?,
            sql(&r.last_error)?,
            sql(&r.next_refresh_ms)?,
            sql(&r.refresh_operation_id)?,
        ],
        update,
        1,
    )
}
const PRICE_OVERRIDE_COLS: &str = "model_id,revision,mode,tariffs_json,updated_by,updated_at";
fn decode_price_override(v: &Value) -> Result<StoredPriceOverride, StorageError> {
    Ok(StoredPriceOverride {
        model_id: field(v, "model_id")?,
        revision: field(v, "revision")?,
        mode: field(v, "mode")?,
        tariffs: serde_json::from_value(json(v, "tariffs_json")?)
            .map_err(|_| corrupt("invalid stored tariffs"))?,
        updated_by: field(v, "updated_by")?,
        updated_at_ms: field(v, "updated_at")?,
    })
}
fn write_price_override(
    c: &Connection,
    r: &StoredPriceOverride,
    update: bool,
) -> Result<(), StorageError> {
    write(
        c,
        "usage_price_overrides",
        PRICE_OVERRIDE_COLS,
        vec![
            sql(&r.model_id)?,
            sql(&r.revision)?,
            sql(&r.mode)?,
            sql(&r.tariffs)?,
            sql(&r.updated_by)?,
            sql(&r.updated_at_ms)?,
        ],
        update,
        1,
    )
}
fn attempt(c: &Connection, id: &Id) -> Result<Option<StoredInferenceAttempt>, StorageError> {
    Ok(rows(
        c,
        &format!("SELECT {ATTEMPT_COLS} FROM usage_attempts WHERE attempt_id=?1"),
        [id.as_str()],
        decode_attempt,
    )?
    .pop())
}
fn observation(
    c: &Connection,
    id: &Id,
    key: &str,
) -> Result<Option<StoredUsageObservation>, StorageError> {
    Ok(rows(c,&format!("SELECT {OBSERVATION_COLS} FROM usage_observations WHERE attempt_id=?1 AND observation_key=?2"),params![id.as_str(),key],decode_observation)?.pop())
}
fn balance(c: &Connection, id: &Id) -> Result<Option<StoredAttemptBalance>, StorageError> {
    Ok(rows(
        c,
        &format!("SELECT {BALANCE_COLS} FROM usage_attempt_balances WHERE attempt_id=?1"),
        [id.as_str()],
        decode_balance,
    )?
    .pop())
}
fn budget(c: &Connection, id: &Id) -> Result<Option<StoredFeatureBudget>, StorageError> {
    Ok(rows(
        c,
        &format!("SELECT {BUDGET_COLS} FROM usage_feature_budgets WHERE feature_id=?1"),
        [id.as_str()],
        decode_budget,
    )?
    .pop())
}
fn origin(c: &Connection, kind: &str, id: &str) -> Result<Option<StoredUsageOrigin>, StorageError> {
    Ok(rows(
        c,
        &format!("SELECT {ORIGIN_COLS} FROM usage_origins WHERE source_kind=?1 AND source_id=?2"),
        params![kind, id],
        decode_origin,
    )?
    .pop())
}
fn snapshot(c: &Connection, id: &Id) -> Result<Option<StoredPriceSnapshot>, StorageError> {
    Ok(rows(
        c,
        &format!("SELECT {SNAPSHOT_COLS} FROM usage_price_snapshots WHERE id=?1"),
        [id.as_str()],
        decode_snapshot,
    )?
    .pop())
}
fn status(c: &Connection, p: &str) -> Result<Option<StoredPriceStatus>, StorageError> {
    Ok(rows(
        c,
        &format!("SELECT {STATUS_COLS} FROM usage_price_status WHERE provider=?1"),
        [p],
        decode_status,
    )?
    .pop())
}
fn price_override(c: &Connection, id: &Id) -> Result<Option<StoredPriceOverride>, StorageError> {
    Ok(rows(c,&format!("SELECT {PRICE_OVERRIDE_COLS} FROM usage_price_overrides WHERE model_id=?1 ORDER BY revision DESC LIMIT 1"),[id.as_str()],decode_price_override)?.pop())
}
fn override_history(
    c: &Connection,
    model: Option<&Id>,
    after: Option<&PriceOverridePosition>,
    limit: u16,
) -> Result<Vec<StoredPriceOverride>, StorageError> {
    let limit = page(limit)?;
    if let Some(position) = after {
        positive(position.revision)?;
    }
    let (model_after, revision_after) =
        after.map_or(("", 0), |p| (p.model_id.as_str(), p.revision));
    match model {
        Some(model) => rows(
            c,
            &format!(
                "SELECT {PRICE_OVERRIDE_COLS} FROM usage_price_overrides WHERE model_id=?3 AND (model_id,revision)>(?1,?2) ORDER BY model_id,revision LIMIT ?4"
            ),
            params![model_after, revision_after, model.as_str(), limit],
            decode_price_override,
        ),
        None => rows(
            c,
            &format!(
                "SELECT {PRICE_OVERRIDE_COLS} FROM usage_price_overrides WHERE (model_id,revision)>(?1,?2) ORDER BY model_id,revision LIMIT ?3"
            ),
            params![model_after, revision_after, limit],
            decode_price_override,
        ),
    }
}
fn price_revision(c: &Connection) -> Result<i64, StorageError> {
    db(c.query_row(
        "SELECT revision FROM usage_price_revision WHERE singleton=1",
        [],
        |r| r.get(0),
    ))
}
fn bootstrap(c: &Connection) -> Result<StoredUsageBootstrap, StorageError> {
    rows(
        c,
        "SELECT * FROM usage_bootstrap WHERE singleton=1",
        [],
        |v| {
            Ok(StoredUsageBootstrap {
                revision: field(v, "revision")?,
                phase: field(v, "phase")?,
                session_after: if v["session_sequence"].is_null() {
                    None
                } else {
                    Some(SequenceKey {
                        sequence: field(v, "session_sequence")?,
                        source: field(v, "session_source")?,
                        id: field(v, "session_id")?,
                    })
                },
                router_after: field(v, "router_after")?,
                job_after: if v["job_run_id"].is_null() {
                    None
                } else {
                    Some(UsageJobPosition {
                        run_id: field(v, "job_run_id")?,
                        seq: field(v, "job_seq")?,
                    })
                },
                source_sequence: field(v, "source_sequence")?,
                processed: field(v, "processed")?,
                completed_at_ms: field(v, "completed_at")?,
            })
        },
    )?
    .pop()
    .ok_or_else(|| corrupt("missing usage bootstrap"))
}
fn range(start: Option<i64>, end: Option<i64>) -> Result<(), StorageError> {
    nonnegative(start)?;
    nonnegative(end)?;
    if start.zip(end).is_some_and(|(a, b)| a > b) {
        Err(invalid("usage time range is inverted"))
    } else {
        Ok(())
    }
}
fn exact_aggregate<T>(result: rusqlite::Result<T>) -> Result<T, StorageError> {
    match result {
        Err(error) if error.to_string().contains("integer overflow") => {
            Err(invalid("usage total exceeds exact micro-USD range"))
        }
        other => db(other),
    }
}
fn feature_totals(c: &Connection, id: &Id) -> Result<StoredFeatureUsageTotals, StorageError> {
    let mut out=exact_aggregate(c.query_row(
        "SELECT count(*), coalesce(sum(b.priced_micro_usd),0),coalesce(sum(b.reserved_micro_usd),0),
         coalesce(sum(CASE WHEN b.classification='ReportedCharge' THEN b.priced_micro_usd ELSE 0 END),0),
         coalesce(sum(CASE WHEN b.classification IN ('ConfiguredEstimate','OfficialEstimate','SubscriptionApiEquivalent') THEN b.priced_micro_usd ELSE 0 END),0),
         coalesce(sum(b.classification='LocalFree'),0),coalesce(sum(b.classification='Untracked'),0),
         coalesce(sum(coalesce(b.monetary_complete,0)=0),0),coalesce(sum(b.unknown_tail=1),0),
         coalesce(sum(b.incomplete=1 OR b.unknown_tail=1),0)
         FROM usage_attempts a LEFT JOIN usage_attempt_balances b ON b.attempt_id=a.attempt_id WHERE a.feature_id=?1",
        [id.as_str()],|r|Ok(StoredFeatureUsageTotals{attempts:r.get(0)?,spent_micro_usd:r.get(1)?,reserved_micro_usd:r.get(2)?,reported_micro_usd:r.get(3)?,estimated_micro_usd:r.get(4)?,local_free_attempts:r.get(5)?,untracked_attempts:r.get(6)?,monetary_incomplete_attempts:r.get(7)?,unknown_tail_attempts:r.get(8)?,incomplete_attempts:r.get(9)?,..StoredFeatureUsageTotals::default()})))?;
    let(legacy,untracked,subtotal)=exact_aggregate(c.query_row(
        "SELECT count(*),coalesce(sum(classification='Untracked'),0),coalesce(sum(priced_micro_usd),0)
         FROM usage_origins WHERE feature_id=?1 AND attempt_id IS NULL AND source_kind IN ('SessionUsage','JobUsage')",[id.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))))?;
    out.legacy_usage_facts = legacy;
    out.legacy_untracked_facts = untracked;
    out.legacy_observation_micro_usd = subtotal;
    Ok(out)
}
/// The attribute filters of a [`UsageAttemptFilter`] over attempt alias `a`, as ?3..?13
/// (project, feature, work, assignment, session, job, -, -, actor, model, connection, purpose).
const ATTEMPT_FILTER: &str = "(?3 IS NULL OR a.project_id=?3) AND (?4 IS NULL OR a.feature_id=?4) AND (?5 IS NULL OR a.work_id=?5) AND (?6 IS NULL OR a.assignment_id=?6) AND (?7 IS NULL OR a.session_id=?7) AND (?8 IS NULL OR a.job_run_id=?8) AND (?10 IS NULL OR a.actor_id=?10) AND (?11 IS NULL OR a.catalog_model_id=?11) AND (?12 IS NULL OR a.connection_id=?12) AND (?13 IS NULL OR a.purpose=?13)";
fn attempt_columns() -> String {
    ATTEMPT_COLS
        .split(',')
        .map(|c| format!("a.{c} AS {c}"))
        .collect::<Vec<_>>()
        .join(",")
}
fn time_range(start: Option<i64>, end: Option<i64>) -> Result<(i64, i64), StorageError> {
    range(start, end)?;
    start
        .zip(end)
        .ok_or_else(|| invalid("a time-ordered usage page needs a start and an end"))
}
fn attempts_between(
    c: &Connection,
    f: &UsageAttemptFilter,
    after: Option<&(i64, Id)>,
    limit: u16,
) -> Result<Vec<StoredInferenceAttempt>, StorageError> {
    let (start, end) = time_range(f.start_ms, f.end_ms)?;
    let (at, id) = after.map_or((start, ""), |(at, id)| (*at, id.as_str()));
    rows(
        c,
        &format!(
            "SELECT {} FROM usage_attempts a WHERE (a.created_at,a.attempt_id)>(?1,?2) AND a.created_at<?9 AND {ATTEMPT_FILTER} ORDER BY a.created_at,a.attempt_id LIMIT ?14",
            attempt_columns()
        ),
        params![
            at,
            id,
            f.project_id.as_ref().map(|v| v.0.as_str()),
            f.feature_id.as_ref().map(Id::as_str),
            f.work_id.as_ref().map(Id::as_str),
            f.assignment_id.as_ref().map(|v| v.0.as_str()),
            f.session_id.as_ref().map(Id::as_str),
            f.job_run_id.as_ref().map(Id::as_str),
            end,
            f.actor_id.as_ref().map(|v| v.0.as_str()),
            f.model_id.as_ref().map(Id::as_str),
            f.connection_id.as_ref().map(Id::as_str),
            f.purpose,
            page(limit)?
        ],
        decode_attempt,
    )
}
fn decode_late(v: &Value) -> Result<(i64, StoredInferenceAttempt), StorageError> {
    Ok((field(v, "request_created_at")?, decode_attempt(v)?))
}
fn late_attempts(
    c: &Connection,
    f: &UsageAttemptFilter,
    after: Option<&(i64, Id, Id)>,
    limit: u16,
) -> Result<Vec<(i64, StoredInferenceAttempt)>, StorageError> {
    let (start, end) = time_range(f.start_ms, f.end_ms)?;
    let (at, request, attempt) = after.map_or((start, "", ""), |(at, request, attempt)| {
        (*at, request.as_str(), attempt.as_str())
    });
    // Driven from the requests created in the range, in their time index order (CROSS JOIN
    // keeps it), and within a request in attempt order: a page resumes after the last
    // (request time, request, attempt) it returned, so no page revisits earlier requests.
    rows(
        c,
        &format!(
            "SELECT r.created_at AS request_created_at,{} FROM router_requests r CROSS JOIN usage_attempts a ON a.request_id=r.request_id WHERE (r.created_at,r.request_id)>=(?1,?15) AND ((r.created_at,r.request_id)>(?1,?15) OR a.attempt_id>?2) AND r.created_at<?9 AND a.created_at>=?9 AND {ATTEMPT_FILTER} ORDER BY r.created_at,r.request_id,a.attempt_id LIMIT ?14",
            attempt_columns()
        ),
        params![
            at,
            attempt,
            f.project_id.as_ref().map(|v| v.0.as_str()),
            f.feature_id.as_ref().map(Id::as_str),
            f.work_id.as_ref().map(Id::as_str),
            f.assignment_id.as_ref().map(|v| v.0.as_str()),
            f.session_id.as_ref().map(Id::as_str),
            f.job_run_id.as_ref().map(Id::as_str),
            end,
            f.actor_id.as_ref().map(|v| v.0.as_str()),
            f.model_id.as_ref().map(Id::as_str),
            f.connection_id.as_ref().map(Id::as_str),
            f.purpose,
            page(limit)?,
            request
        ],
        decode_late,
    )
}
fn origins_between(
    c: &Connection,
    f: &UsageOriginFilter,
    after: Option<&(i64, String, String)>,
    limit: u16,
) -> Result<Vec<StoredUsageOrigin>, StorageError> {
    let (start, end) = time_range(f.start_ms, f.end_ms)?;
    let (at, kind, id) = after.map_or((start, "", ""), |(at, kind, id)| {
        (*at, kind.as_str(), id.as_str())
    });
    rows(
        c,
        &format!(
            "SELECT {ORIGIN_COLS} FROM usage_origins WHERE (recorded_at,source_kind,source_id)>(?1,?2,?3) AND recorded_at<?4 AND (?5 IS NULL OR project_id=?5) AND (?6 IS NULL OR feature_id=?6) AND (?7 IS NULL OR source_kind=?7) ORDER BY recorded_at,source_kind,source_id LIMIT ?8"
        ),
        params![
            at,
            kind,
            id,
            end,
            f.project_id.as_ref().map(|v| v.0.as_str()),
            f.feature_id.as_ref().map(Id::as_str),
            f.kind,
            page(limit)?
        ],
        decode_origin,
    )
}
macro_rules! usage_reads{($t:ty)=>{impl UsageRead for $t{
 fn inference_attempts_between(&self,f:&UsageAttemptFilter,after:Option<&(i64,Id)>,limit:u16)->Result<Vec<StoredInferenceAttempt>,StorageError>{attempts_between(self.conn,f,after,limit)}
 fn late_inference_attempts(&self,f:&UsageAttemptFilter,after:Option<&(i64,Id,Id)>,limit:u16)->Result<Vec<(i64,StoredInferenceAttempt)>,StorageError>{late_attempts(self.conn,f,after,limit)}
 fn usage_origins_between(&self,f:&UsageOriginFilter,after:Option<&(i64,String,String)>,limit:u16)->Result<Vec<StoredUsageOrigin>,StorageError>{origins_between(self.conn,f,after,limit)}
 fn inference_attempt(&self,id:&Id)->Result<Option<StoredInferenceAttempt>,StorageError>{attempt(self.conn,id)}
 fn inference_attempts(&self,f:&UsageAttemptFilter,after:Option<&Id>,limit:u16)->Result<Vec<StoredInferenceAttempt>,StorageError>{range(f.start_ms,f.end_ms)?;rows(self.conn,&format!("SELECT {ATTEMPT_COLS} FROM usage_attempts WHERE attempt_id>?1 AND (?2 IS NULL OR project_id=?2) AND (?3 IS NULL OR feature_id=?3) AND (?4 IS NULL OR work_id=?4) AND (?5 IS NULL OR assignment_id=?5) AND (?6 IS NULL OR session_id=?6) AND (?7 IS NULL OR job_run_id=?7) AND (?8 IS NULL OR created_at>=?8) AND (?9 IS NULL OR created_at<?9) AND (?10 IS NULL OR actor_id=?10) AND (?11 IS NULL OR catalog_model_id=?11) AND (?12 IS NULL OR connection_id=?12) AND (?13 IS NULL OR purpose=?13) ORDER BY attempt_id LIMIT ?14"),params![after.map_or("",Id::as_str),f.project_id.as_ref().map(|v|v.0.as_str()),f.feature_id.as_ref().map(Id::as_str),f.work_id.as_ref().map(Id::as_str),f.assignment_id.as_ref().map(|v|v.0.as_str()),f.session_id.as_ref().map(Id::as_str),f.job_run_id.as_ref().map(Id::as_str),f.start_ms,f.end_ms,f.actor_id.as_ref().map(|v|v.0.as_str()),f.model_id.as_ref().map(Id::as_str),f.connection_id.as_ref().map(Id::as_str),f.purpose,page(limit)?],decode_attempt)}
 fn unquiesced_attempts(&self,after:Option<&Id>,limit:u16)->Result<Vec<StoredInferenceAttempt>,StorageError>{let cols=attempt_columns();rows(self.conn,&format!("SELECT {cols} FROM usage_attempt_balances b JOIN usage_attempts a ON a.attempt_id=b.attempt_id WHERE b.local_quiescent=0 AND b.attempt_id>?1 ORDER BY b.attempt_id LIMIT ?2"),params![after.map_or("",Id::as_str),page(limit)?],decode_attempt)}
 fn usage_observation(&self,id:&Id,key:&str)->Result<Option<StoredUsageObservation>,StorageError>{observation(self.conn,id,key)}
 fn usage_observations(&self,id:&Id,after:Option<i64>,limit:u16)->Result<Vec<StoredUsageObservation>,StorageError>{nonnegative(after)?;rows(self.conn,&format!("SELECT {OBSERVATION_COLS} FROM usage_observations WHERE attempt_id=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3"),params![id.as_str(),after.unwrap_or(0),page(limit)?],decode_observation)}
 fn attempt_balance(&self,id:&Id)->Result<Option<StoredAttemptBalance>,StorageError>{balance(self.conn,id)}
 fn feature_budget(&self,id:&Id)->Result<Option<StoredFeatureBudget>,StorageError>{budget(self.conn,id)}
 fn feature_usage_totals(&self,id:&Id)->Result<StoredFeatureUsageTotals,StorageError>{feature_totals(self.conn,id)}
 fn usage_origins(&self,f:&UsageOriginFilter,after:Option<&str>,limit:u16)->Result<Vec<StoredUsageOrigin>,StorageError>{range(f.start_ms,f.end_ms)?;let key:Option<(String,String)>=after.map(|s|serde_json::from_str(s).map_err(|_|invalid("invalid usage origin cursor"))).transpose()?;let(k,i)=key.as_ref().map_or(("",""),|(k,i)|(k.as_str(),i.as_str()));rows(self.conn,&format!("SELECT {ORIGIN_COLS} FROM usage_origins WHERE (source_kind,source_id)>(?1,?2) AND (?3 IS NULL OR project_id=?3) AND (?4 IS NULL OR feature_id=?4) AND (?5 IS NULL OR recorded_at>=?5) AND (?6 IS NULL OR recorded_at<?6) AND (?7 IS NULL OR source_kind=?7) ORDER BY source_kind,source_id LIMIT ?8"),params![k,i,f.project_id.as_ref().map(|v|v.0.as_str()),f.feature_id.as_ref().map(Id::as_str),f.start_ms,f.end_ms,f.kind,page(limit)?],decode_origin)}
 fn usage_origin(&self,kind:&str,id:&str)->Result<Option<StoredUsageOrigin>,StorageError>{origin(self.conn,kind,id)}
 fn usage_changes(&self,feature:Option<&Id>,after:Option<&Id>,limit:u16)->Result<Vec<StoredUsageChange>,StorageError>{rows(self.conn,&format!("SELECT {CHANGE_COLS} FROM usage_changes WHERE event_id>?1 AND (?2 IS NULL OR feature_id=?2) ORDER BY event_id LIMIT ?3"),params![after.map_or("",Id::as_str),feature.map(Id::as_str),page(limit)?],decode_change)}
 fn usage_bootstrap(&self)->Result<StoredUsageBootstrap,StorageError>{bootstrap(self.conn)}
 fn price_revision(&self)->Result<i64,StorageError>{price_revision(self.conn)}
 fn price_snapshots(&self,provider:Option<&str>,after:Option<&Id>,limit:u16)->Result<Vec<StoredPriceSnapshot>,StorageError>{if let Some(p)=provider{bounded(p,128)?;}rows(self.conn,&format!("SELECT {SNAPSHOT_COLS} FROM usage_price_snapshots WHERE id>?1 AND (?2 IS NULL OR provider=?2) ORDER BY id LIMIT ?3"),params![after.map_or("",Id::as_str),provider,page(limit)?],decode_snapshot)}
 fn price_snapshot(&self,id:&Id)->Result<Option<StoredPriceSnapshot>,StorageError>{snapshot(self.conn,id)}
 fn current_price_snapshot(&self,p:&str)->Result<Option<StoredPriceSnapshot>,StorageError>{status(self.conn,p)?.and_then(|s|s.current_snapshot_id).map(|id|snapshot(self.conn,&id)).transpose().map(Option::flatten)}
 fn price_status(&self,p:&str)->Result<Option<StoredPriceStatus>,StorageError>{status(self.conn,p)}
 fn price_override_history(&self,model:Option<&Id>,after:Option<&PriceOverridePosition>,limit:u16)->Result<Vec<StoredPriceOverride>,StorageError>{override_history(self.conn,model,after,limit)}
 fn price_override(&self,id:&Id)->Result<Option<StoredPriceOverride>,StorageError>{price_override(self.conn,id)}
 fn price_override_revision(&self,id:&Id)->Result<i64,StorageError>{Ok(price_override(self.conn,id)?.map_or(1,|v|v.revision))}
}};}
usage_reads!(Unit<'_>);
usage_reads!(Snapshot<'_>);
fn state(s: &str) -> Result<(), StorageError> {
    if [
        "Admitted",
        "Dispatched",
        "Settled",
        "UnknownTail",
        "CancelledBeforeDispatch",
    ]
    .contains(&s)
    {
        Ok(())
    } else {
        Err(invalid("invalid inference attempt state"))
    }
}
fn missing(kind: &'static str, id: &Id) -> StorageError {
    StorageError::NotFound {
        kind,
        id: id.as_str().into(),
    }
}
fn validate_binding(unit: &Unit<'_>, r: &StoredInferenceAttempt) -> Result<(), StorageError> {
    let journal = OperationRead::operation(unit, &r.operation_id)?
        .ok_or_else(|| missing("operation", &r.operation_id.0))?;
    if !journal
        .attempts
        .iter()
        .any(|a| a.id.0 == r.attempt_id && a.attempt_no == r.attempt_number)
    {
        return Err(invalid(
            "canonical usage identity must match its journal attempt",
        ));
    }
    let route = RouterRead::route_request(unit, &r.request_id)?
        .ok_or_else(|| missing("router request", &r.request_id))?;
    if route.operation_id != r.operation_id
        || route.project_id != r.project_id
        || route.session_id != r.session_id
        || route.purpose != r.purpose
    {
        return Err(invalid("canonical usage request ownership mismatch"));
    }
    if journal
        .assignment_id
        .as_ref()
        .is_some_and(|id| Some(id) != r.assignment_id.as_ref())
    {
        return Err(invalid("canonical usage journal assignment mismatch"));
    }
    if journal.root_id.as_ref().is_some_and(|root| {
        Some(&root.0) != r.work_id.as_ref() && Some(&root.0) != r.feature_id.as_ref()
    }) {
        return Err(invalid("canonical usage journal root mismatch"));
    }

    for (id, feature) in [(r.feature_id.as_ref(), true), (r.work_id.as_ref(), false)] {
        if let Some(id) = id {
            let work = WorkRead::work(unit, id)?.ok_or_else(|| missing("work", id))?;
            if Some(&work.project_id) != r.project_id.as_ref() || (feature && work.kind != FEATURE)
            {
                return Err(invalid("attempt work ownership mismatch"));
            }
        }
    }
    match (&r.execution_root_id, &r.execution_id) {
        (None, None) => {
            if r.assignment_id.is_some() {
                return Err(invalid(
                    "managed assignment requires captured execution ownership",
                ));
            }
        }
        (Some(root), Some(execution)) => {
            let activation = RunRead::activation(unit, execution)?
                .ok_or_else(|| missing("activation", execution))?;
            if activation.root_id != *root {
                return Err(invalid("captured execution root does not match activation"));
            }
            if r.work_id.as_ref() != Some(&root.0) && r.feature_id.as_ref() != Some(&root.0) {
                return Err(invalid("captured execution does not own admitted work"));
            }
            let session = r
                .session_id
                .as_ref()
                .ok_or_else(|| invalid("managed execution requires its durable session"))?;
            let session =
                SessionRead::session(unit, session)?.ok_or_else(|| missing("session", session))?;
            if session.root_id.as_ref() != Some(root) {
                return Err(invalid("managed execution session root mismatch"));
            }
            let work = WorkRead::work(unit, &root.0)?.ok_or_else(|| missing("work", &root.0))?;
            if r.project_id.as_ref() != Some(&work.project_id) {
                return Err(invalid("captured execution project mismatch"));
            }
            if let Some(assignment) = &r.assignment_id {
                let a = RunRead::assignment(unit, assignment)?
                    .ok_or_else(|| missing("assignment", &assignment.0))?;
                if a.root_id != *root || a.execution_id.as_ref() != Some(execution) {
                    return Err(invalid("captured assignment execution mismatch"));
                }
            }
        }
        _ => {
            return Err(invalid(
                "captured execution root and activation must be paired",
            ));
        }
    }
    if let Some(id) = &r.assignment_id {
        let a = RunRead::assignment(unit, id)?.ok_or_else(|| missing("assignment", &id.0))?;
        if r.work_id.as_ref() != Some(&a.work_id.0) || r.session_id.as_ref() != Some(&a.session_id)
        {
            return Err(invalid("attempt assignment ownership mismatch"));
        }
    }
    if let Some(id) = &r.session_id {
        let s = SessionRead::session(unit, id)?.ok_or_else(|| missing("session", id))?;
        if s.project_id != r.project_id {
            return Err(invalid("attempt session project mismatch"));
        }
    }
    if let Some(id) = &r.job_run_id {
        let run = JobRead::job_run(unit, id)?.ok_or_else(|| missing("job run", id))?;
        if run.project_id.is_some() && run.project_id != r.project_id {
            return Err(invalid("attempt job project scope mismatch"));
        }
    }
    if let Some(work) = &r.work_id {
        let w = WorkRead::work(unit, work)?.ok_or_else(|| missing("work", work))?;
        if let Some(feature) = &r.feature_id
            && &w.id != feature
            && w.parent_feature_id.as_ref() != Some(feature)
        {
            return Err(invalid("attempt feature membership snapshot mismatch"));
        }
    }
    Ok(())
}
fn validate_quote(unit: &Unit<'_>, r: &StoredInferenceAttempt) -> Result<(), StorageError> {
    let conn = RouterRead::connection(unit, &r.connection_id)?
        .ok_or_else(|| missing("connection", &r.connection_id))?;
    if conn.profile != r.endpoint_profile {
        return Err(invalid("attempt endpoint profile mismatch"));
    }
    let q = &r.pricing_snapshot["quote"];
    if q["connection_revision"].as_i64() != Some(conn.revision) {
        return Err(conflict(
            r.connection_id.as_str(),
            q["connection_revision"].as_i64().unwrap_or(1),
            conn.revision,
        ));
    }
    if q["classification"] == "LocalFree" && !conn.local_free {
        return Err(invalid(
            "LocalFree requires explicit connection billing policy",
        ));
    }
    let current = price_override(unit.conn, &r.catalog_model_id)?.map_or(1, |v| v.revision);
    if q["override_revision"].as_i64().unwrap_or(1) != current {
        return Err(conflict(
            r.catalog_model_id.as_str(),
            q["override_revision"].as_i64().unwrap_or(1),
            current,
        ));
    }
    if let Some(id) = q["snapshot_id"].as_str() {
        let id = Id::new(id).map_err(|_| invalid("invalid quote snapshot id"))?;
        let s = snapshot(unit.conn, &id)?.ok_or_else(|| missing("price snapshot", &id))?;
        let st = status(unit.conn, &s.provider)?
            .ok_or_else(|| invalid("quote provider has no publication"))?;
        if st.current_snapshot_id.as_ref() != Some(&id)
            || q["price_revision"].as_i64() != Some(s.revision)
        {
            return Err(invalid("quote price snapshot is no longer current"));
        }
    }
    Ok(())
}
fn validate_origin_scope(unit: &Unit<'_>, r: &StoredUsageOrigin) -> Result<(), StorageError> {
    for (id, feature) in [(r.feature_id.as_ref(), true), (r.work_id.as_ref(), false)] {
        if let Some(id) = id {
            let work = WorkRead::work(unit, id)?.ok_or_else(|| missing("work", id))?;
            if r.project_id.as_ref().is_some_and(|p| p != &work.project_id)
                || (feature && work.kind != FEATURE)
            {
                return Err(invalid(
                    "legacy usage source has conflicting known project/feature ownership",
                ));
            }
        }
    }
    if let Some(id) = &r.session_id {
        let session = SessionRead::session(unit, id)?.ok_or_else(|| missing("session", id))?;
        if r.project_id
            .as_ref()
            .is_some_and(|p| Some(p) != session.project_id.as_ref())
        {
            return Err(invalid("legacy usage source session project mismatch"));
        }
    }
    if let Some(id) = &r.job_run_id {
        let run = JobRead::job_run(unit, id)?.ok_or_else(|| missing("job run", id))?;
        if r.project_id
            .as_ref()
            .is_some_and(|p| Some(p) != run.project_id.as_ref())
        {
            return Err(invalid("legacy usage source job project mismatch"));
        }
    }
    Ok(())
}
fn validate_origin_binding(
    o: &StoredUsageOrigin,
    a: &StoredInferenceAttempt,
) -> Result<(), StorageError> {
    if o.request_id.as_ref().is_some_and(|v| v != &a.request_id)
        || o.project_id
            .as_ref()
            .is_some_and(|v| Some(v) != a.project_id.as_ref())
        || o.feature_id
            .as_ref()
            .is_some_and(|v| Some(v) != a.feature_id.as_ref())
        || o.work_id
            .as_ref()
            .is_some_and(|v| Some(v) != a.work_id.as_ref())
        || o.session_id
            .as_ref()
            .is_some_and(|v| Some(v) != a.session_id.as_ref())
        || o.job_run_id
            .as_ref()
            .is_some_and(|v| Some(v) != a.job_run_id.as_ref())
    {
        return Err(invalid(
            "usage origin binding conflicts with immutable attempt ownership",
        ));
    }
    Ok(())
}
fn atomic<T>(
    c: &Connection,
    f: impl FnOnce() -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    db(c.execute_batch("SAVEPOINT usage_mutation"))?;
    match f() {
        Ok(v) => {
            db(c.execute_batch("RELEASE usage_mutation"))?;
            Ok(v)
        }
        Err(e) => {
            db(c.execute_batch("ROLLBACK TO usage_mutation; RELEASE usage_mutation"))?;
            Err(e)
        }
    }
}
fn advance(c: &Connection, expected: i64) -> Result<i64, StorageError> {
    let current = price_revision(c)?;
    if current != expected {
        return Err(conflict("prices", expected, current));
    }
    let next = current
        .checked_add(1)
        .ok_or_else(|| invalid("price revision overflow"))?;
    db(c.execute(
        "UPDATE usage_price_revision SET revision=?1 WHERE singleton=1 AND revision=?2",
        params![next, expected],
    ))?;
    Ok(next)
}
fn tariffs(r: &[devworks_core::ports::usage::PublishedTariff]) -> Result<(), StorageError> {
    if r.len() > 100_000
        || serde_json::to_vec(r)
            .map_err(|_| invalid("tariff serialization"))?
            .len()
            > 32 * 1024 * 1024
    {
        return Err(invalid("tariff catalog exceeds bound"));
    }
    for t in r {
        provider_model_text(&t.upstream_model_id)?;
        bounded(&t.service_tier, 128)?;
        bounded(&t.modality, 64)?;
        bounded(&t.charge_kind, 64)?;
        bounded(&t.unit, 64)?;
        if t.units <= 0 {
            return Err(invalid("tariff unit count must be positive"));
        }
        let mut amount = devworks_core::services::usage::money::CostAccumulator::default();
        amount
            .amount_usd(&t.amount_usd)
            .map_err(|_| invalid("invalid exact tariff amount"))?;
    }
    Ok(())
}
impl UsageRepository for Unit<'_> {
    fn insert_inference_attempt(&mut self, r: &StoredInferenceAttempt) -> Result<(), StorageError> {
        positive(r.attempt_number)?;
        if r.revision != 1 || r.state != "Admitted" {
            return Err(invalid("attempt starts admitted at revision1"));
        }
        nonnegative(Some(r.created_at_ms))?;
        for s in [&r.role, &r.purpose, &r.endpoint_profile, &r.access_mode] {
            bounded(s, 256)?;
        }
        object(&r.pricing_snapshot)?;
        if let Some(value) = &r.provider_model_id {
            provider_model_text(value)?;
        }
        if let Some(value) = r
            .pricing_snapshot
            .get("quoted_provider_model_id")
            .filter(|v| !v.is_null())
        {
            provider_model_text(
                value
                    .as_str()
                    .ok_or_else(|| invalid("quoted provider model identity must be Text"))?,
            )?;
        }
        validate_binding(self, r)?;
        validate_quote(self, r)?;
        write_attempt(self.conn, r, false)
    }
    fn update_inference_attempt(
        &mut self,
        r: &StoredInferenceAttempt,
        expected: i64,
    ) -> Result<(), StorageError> {
        let old =
            attempt(self.conn, &r.attempt_id)?.ok_or_else(|| missing("attempt", &r.attempt_id))?;
        cas(
            r.attempt_id.as_str(),
            Some(old.revision),
            Some(expected),
            r.revision,
        )?;
        state(&r.state)?;
        let mut permitted = old.clone();
        permitted.state = r.state.clone();
        permitted.provider_model_id = r.provider_model_id.clone();
        permitted.revision = r.revision;
        if permitted != *r {
            return Err(invalid("attempt admission binding is immutable"));
        }
        if let Some(v) = &r.provider_model_id {
            provider_model_text(v)?;
        }
        if r.state != old.state
            && !matches!(
                (old.state.as_str(), r.state.as_str()),
                (
                    "Admitted",
                    "Dispatched" | "CancelledBeforeDispatch" | "UnknownTail"
                ) | ("Dispatched", "Settled" | "UnknownTail")
                    | ("UnknownTail", "Settled")
            )
        {
            return Err(invalid("invalid attempt state transition"));
        }
        write_attempt(self.conn, r, true)
    }
    fn insert_usage_observation(&mut self, r: &StoredUsageObservation) -> Result<(), StorageError> {
        bounded(&r.observation_key, 256)?;
        positive(r.sequence)?;
        digest(&r.digest)?;
        object(&r.payload)?;
        nonnegative(Some(r.recorded_at_ms))?;
        if let Some(old) = observation(self.conn, &r.attempt_id, &r.observation_key)? {
            return if old == *r {
                Ok(())
            } else {
                Err(invalid("immutable observation key collision"))
            };
        }
        write_observation(self.conn, r, false)
    }
    fn put_attempt_balance(
        &mut self,
        r: &StoredAttemptBalance,
        expected: Option<i64>,
    ) -> Result<(), StorageError> {
        let prior = balance(self.conn, &r.attempt_id)?;
        cas(
            r.attempt_id.as_str(),
            prior.as_ref().map(|v| v.revision),
            expected,
            r.revision,
        )?;
        if prior.is_some_and(|v| v.local_quiescent) && !r.local_quiescent {
            return Err(invalid(
                "a locally closed attempt cannot reopen its transport",
            ));
        }
        nonnegative(r.priced_micro_usd)?;
        nonnegative(r.reserved_micro_usd)?;
        classification(&r.classification)?;
        object(&r.summary)?;
        if r.classification == "LocalFree"
            && (r.priced_micro_usd != Some(0) || !r.monetary_complete)
        {
            return Err(invalid(
                "LocalFree balance must be explicitly zero and complete",
            ));
        }
        write_balance(self.conn, r, true)
    }
    fn put_feature_budget(
        &mut self,
        r: &StoredFeatureBudget,
        expected: Option<i64>,
    ) -> Result<(), StorageError> {
        let w = WorkRead::work(self, &r.feature_id)?
            .ok_or_else(|| missing("feature", &r.feature_id))?;
        if w.kind != FEATURE {
            return Err(invalid("only features own budgets"));
        }
        cas(
            r.feature_id.as_str(),
            budget(self.conn, &r.feature_id)?.map(|v| v.revision),
            expected,
            r.revision,
        )?;
        nonnegative(r.limit_micro_usd)?;
        if r.warning_revision.is_some_and(|n| n <= 0 || n > r.revision) {
            return Err(invalid("invalid feature warning revision"));
        }
        write_budget(self.conn, r, true)
    }
    fn insert_usage_origin(&mut self, r: &StoredUsageOrigin) -> Result<(), StorageError> {
        if ![
            "SessionUsage",
            "JobUsage",
            "ToolInvocation",
            "RouterRequest",
        ]
        .contains(&r.source_kind.as_str())
        {
            return Err(invalid("invalid usage origin kind"));
        }
        bounded(&r.source_id, 512)?;
        object(&r.payload)?;
        classification(&r.classification)?;
        nonnegative(r.priced_micro_usd)?;
        nonnegative(Some(r.recorded_at_ms))?;
        validate_origin_scope(self, r)?;
        if let Some(id) = &r.attempt_id {
            let a = attempt(self.conn, id)?.ok_or_else(|| missing("attempt", id))?;
            validate_origin_binding(r, &a)?;
        }
        if let Some(old) = origin(self.conn, &r.source_kind, &r.source_id)? {
            let mut replay = r.clone();
            replay.attempt_id = old.attempt_id.clone();
            if old != replay {
                return Err(invalid("immutable usage source collision"));
            }
            return match (&old.attempt_id, &r.attempt_id) {
                (Some(a), Some(b)) if a != b => Err(invalid("usage origin already bound")),
                (None, Some(a)) => self.bind_usage_origin(&r.source_kind, &r.source_id, a),
                _ => Ok(()),
            };
        }
        write_origin(self.conn, r, false)
    }
    fn bind_usage_origin(
        &mut self,
        kind: &str,
        id: &str,
        attempt_id: &Id,
    ) -> Result<(), StorageError> {
        let o = origin(self.conn, kind, id)?.ok_or_else(|| invalid("usage origin not found"))?;
        if o.attempt_id.as_ref() == Some(attempt_id) {
            return Ok(());
        }
        if o.attempt_id.is_some() {
            return Err(invalid("usage origin already bound"));
        }
        let a = attempt(self.conn, attempt_id)?.ok_or_else(|| missing("attempt", attempt_id))?;
        validate_origin_binding(&o, &a)?;
        db(self.conn.execute("UPDATE usage_origins SET attempt_id=?1 WHERE source_kind=?2 AND source_id=?3 AND attempt_id IS NULL",params![attempt_id.as_str(),kind,id]))?;
        Ok(())
    }
    fn append_usage_change(&mut self, r: &StoredUsageChange) -> Result<(), StorageError> {
        bounded(&r.kind, 128)?;
        object(&r.payload)?;
        nonnegative(Some(r.created_at_ms))?;
        if let Some(id) = &r.attempt_id {
            let a = attempt(self.conn, id)?.ok_or_else(|| missing("attempt", id))?;
            if r.feature_id != a.feature_id {
                return Err(invalid("usage change feature differs from attempt"));
            }
        }
        write_change(self.conn, r, false)
    }
    fn update_usage_bootstrap(
        &mut self,
        r: &StoredUsageBootstrap,
        expected: i64,
    ) -> Result<(), StorageError> {
        let old = bootstrap(self.conn)?;
        cas(
            "usage-bootstrap",
            Some(old.revision),
            Some(expected),
            r.revision,
        )?;
        let phase = |s: &str| match s {
            "Sessions" => Some(0),
            "Jobs" => Some(1),
            "RouterRequests" => Some(2),
            "Complete" => Some(3),
            _ => None,
        };
        if let Some(cursor) = &r.session_after {
            nonnegative(Some(cursor.sequence))?;
            if ![1, 3].contains(&cursor.source) {
                return Err(invalid("invalid session usage source cursor"));
            }
            bounded(&cursor.id, 512)?;
        }
        if let Some(cursor) = &r.job_after {
            positive(cursor.seq)?;
        }
        let next = phase(&r.phase).ok_or_else(|| invalid("invalid bootstrap phase"))?;
        if next > phase(&old.phase).expect("stored phase") + 1
            || next < phase(&old.phase).expect("stored phase")
            || r.processed < old.processed
            || r.source_sequence < old.source_sequence
            || (r.phase == "Complete") != r.completed_at_ms.is_some()
            || old
                .session_after
                .as_ref()
                .zip(r.session_after.as_ref())
                .is_some_and(|(a, b)| b < a)
            || old.session_after.is_some() && r.session_after.is_none()
        {
            return Err(invalid("usage bootstrap never moves backwards"));
        }
        if old
            .job_after
            .as_ref()
            .zip(r.job_after.as_ref())
            .is_some_and(|(a, b)| (b.run_id.as_str(), b.seq) < (a.run_id.as_str(), a.seq))
            || old.job_after.is_some() && r.job_after.is_none()
        {
            return Err(invalid("job bootstrap cursor never moves backwards"));
        }
        if old
            .router_after
            .as_ref()
            .zip(r.router_after.as_ref())
            .is_some_and(|(a, b)| b.as_str() < a.as_str())
            || old.router_after.is_some() && r.router_after.is_none()
        {
            return Err(invalid("router bootstrap cursor never moves backwards"));
        }
        nonnegative(r.completed_at_ms)?;
        db(self.conn.execute("UPDATE usage_bootstrap SET revision=?1,phase=?2,session_sequence=?3,session_source=?4,session_id=?5,job_run_id=?6,job_seq=?7,source_sequence=?8,processed=?9,completed_at=?10,router_after=?11 WHERE singleton=1",params![r.revision,r.phase,r.session_after.as_ref().map(|s|s.sequence),r.session_after.as_ref().map(|s|s.source),r.session_after.as_ref().map(|s|s.id.as_str()),r.job_after.as_ref().map(|s|s.run_id.as_str()),r.job_after.as_ref().map(|s|s.seq),r.source_sequence,r.processed,r.completed_at_ms,r.router_after.as_ref().map(Id::as_str)]))?;
        Ok(())
    }
    fn advance_price_revision(&mut self, expected: i64) -> Result<i64, StorageError> {
        advance(self.conn, expected)
    }
    fn put_price_status(
        &mut self,
        r: &StoredPriceStatus,
        expected: Option<i64>,
    ) -> Result<(), StorageError> {
        bounded(&r.provider, 128)?;
        let old = status(self.conn, &r.provider)?;
        cas(
            &r.provider,
            old.as_ref().map(|v| v.revision),
            expected,
            r.revision,
        )?;
        if old.as_ref().and_then(|s| s.current_snapshot_id.as_ref())
            != r.current_snapshot_id.as_ref()
            || old.as_ref().and_then(|s| s.last_success_ms) != r.last_success_ms
        {
            return Err(invalid("only publication changes last-good price identity"));
        }
        if let Some(prior) = old.as_ref().and_then(|s| s.refresh_operation_id.as_ref())
            && (r.refresh_operation_id.is_none()
                || (!r.refresh_pending && r.refresh_operation_id.as_ref() != Some(prior)))
        {
            return Err(invalid(
                "terminal price status retains its owning refresh operation",
            ));
        }
        for n in [r.last_attempt_ms, r.last_success_ms, r.next_refresh_ms] {
            nonnegative(n)?;
        }
        if let Some(s) = &r.last_error {
            bounded(s, 2048)?;
        }
        atomic(self.conn, || {
            write_status(self.conn, r, true)?;
            advance(self.conn, price_revision(self.conn)?)?;
            Ok(())
        })
    }
    fn publish_price_snapshot(
        &mut self,
        r: &StoredPriceSnapshot,
        expected: i64,
    ) -> Result<StoredPriceStatus, StorageError> {
        bounded(&r.provider, 128)?;
        bounded(&r.source_url, 2048)?;
        bounded(&r.parser_version, 128)?;
        digest(&r.source_digest)?;
        tariffs(&r.tariffs)?;
        nonnegative(Some(r.fetched_at_ms))?;
        nonnegative(Some(r.verified_at_ms))?;
        let old = status(self.conn, &r.provider)?
            .ok_or_else(|| invalid("price publication requires an owned refresh"))?;
        if old.revision != expected {
            return Err(conflict(&r.provider, expected, old.revision));
        }
        if !old.refresh_pending {
            return Err(invalid("price publication requires pending refresh"));
        }
        let prev = old
            .current_snapshot_id
            .as_ref()
            .map(|id| snapshot(self.conn, id))
            .transpose()?
            .flatten();
        let next = prev.map_or(Ok(1), |s| {
            s.revision
                .checked_add(1)
                .ok_or_else(|| invalid("snapshot revision overflow"))
        })?;
        if r.revision != next {
            return Err(invalid(
                "snapshot revision must advance provider publication",
            ));
        }
        let mut status = old;
        status.revision = expected
            .checked_add(1)
            .ok_or_else(|| invalid("price status revision overflow"))?;
        status.current_snapshot_id = Some(r.id.clone());
        status.last_success_ms = Some(r.verified_at_ms);
        status.refresh_pending = false;
        status.last_error = None;
        atomic(self.conn, || {
            write_snapshot(self.conn, r, false)?;
            write_status(self.conn, &status, true)?;
            advance(self.conn, price_revision(self.conn)?)?;
            Ok(status.clone())
        })
    }
    fn put_price_override(
        &mut self,
        r: &StoredPriceOverride,
        expected: i64,
    ) -> Result<(), StorageError> {
        let current = price_override(self.conn, &r.model_id)?.map_or(1, |v| v.revision);
        if expected != current {
            return Err(conflict(r.model_id.as_str(), expected, current));
        }
        if expected.checked_add(1) != Some(r.revision) {
            return Err(invalid("override revision must advance"));
        }
        if RouterRead::route_model(self, &r.model_id)?.is_none()
            && RouterRead::decision_model(self, &r.model_id)?.is_none()
            && EmbeddingRead::embedding_price_subject(self, &r.model_id)?
                .is_none_or(|subject| subject.current_candidate_id.is_none())
        {
            return Err(missing("model", &r.model_id));
        }
        if !["Set", "Clear"].contains(&r.mode.as_str())
            || (r.mode == "Clear" && !r.tariffs.is_empty())
            || (r.mode == "Set" && r.tariffs.is_empty())
        {
            return Err(invalid("invalid Set/Clear override"));
        }
        tariffs(&r.tariffs)?;
        nonnegative(Some(r.updated_at_ms))?;
        atomic(self.conn, || {
            write_price_override(self.conn, r, false)?;
            advance(self.conn, price_revision(self.conn)?)?;
            Ok(())
        })
    }
}
