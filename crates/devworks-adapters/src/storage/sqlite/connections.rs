//! SQLite provider connections and SystemOne decision models (migration 15):
//! `router_connections` and `router_decision_models`. No secret is ever stored: a connection
//! names its credential by an opaque handle and a credential revision.
use super::conn::db;
use devworks_core::ports::storage::*;
use devworks_core::protocol::{Id, Revision, RevisionDimension};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};

fn invalid(m: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(m.into())
}
fn corrupt(m: impl Into<String>) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, m)
}
fn id_of(s: String) -> Result<Id, StorageError> {
    Id::new(s).map_err(|e| corrupt(format!("stored identifier: {e}")))
}
fn conflict(kind: &'static str, id: &Id, expected: i64, current: i64) -> StorageError {
    StorageError::conflict(
        RevisionDimension::Configuration,
        kind,
        id.clone(),
        Revision::new(expected.max(1)).expect("positive"),
        Revision::new(current.max(1)).expect("positive"),
    )
}
/// The next revision of a compare-and-set insert (`expected` `None`) or replacement.
fn next_revision(
    kind: &'static str,
    id: &Id,
    expected: Option<i64>,
    current: Option<i64>,
) -> Result<i64, StorageError> {
    match (expected, current) {
        (None, None) => Ok(1),
        (None, Some(_)) => Err(StorageError::Duplicate {
            kind,
            id: id.as_str().to_owned(),
        }),
        (Some(_), None) => Err(StorageError::NotFound {
            kind,
            id: id.as_str().to_owned(),
        }),
        (Some(e), Some(c)) if e != c => Err(conflict(kind, id, e, c)),
        (Some(e), Some(_)) => e.checked_add(1).ok_or_else(|| invalid("revision overflow")),
    }
}

const PROFILES: [&str; 8] = [
    "OpenAiResponses",
    "CodexSubscriptionResponses",
    "AnthropicMessages",
    "ClaudeSubscriptionMessages",
    "OpenRouterChat",
    "CompatibleChat",
    "Embedding",
    "SystemOne",
];
const QUESTION_TYPES: [&str; 4] = ["choice", "noul", "score", "box"];

// ---------------------------------------------------------------------------------------------
// Connections

const CONNECTION_COLUMNS: &str = "id,name,profile,endpoint,credential_ref,credential_revision,credential_epoch,lifecycle,state_reason,account_digest,access_expires_at,risk_notice_sha256,risk_binding,risk_restore_epoch,risk_accepted_by,risk_accepted_at,discovered_json,discovered_at,healthy,checked_at,revision,created_at,updated_at,enabled,open_router_json,local_free,account_revision";

struct RawConnection {
    id: String,
    name: String,
    profile: String,
    endpoint: String,
    credential_ref: Option<String>,
    credential_revision: i64,
    account_revision: i64,
    credential_epoch: Option<i64>,
    lifecycle: String,
    state_reason: Option<String>,
    account_digest: Option<String>,
    access_expires_at_ms: Option<i64>,
    risk_notice: Option<String>,
    risk_binding: Option<String>,
    risk_epoch: Option<i64>,
    risk_by: Option<String>,
    risk_at: Option<i64>,
    discovered: Option<String>,
    discovered_at_ms: Option<i64>,
    healthy: Option<bool>,
    checked_at_ms: Option<i64>,
    revision: i64,
    created_at_ms: i64,
    updated_at_ms: i64,
    enabled: bool,
    open_router: Option<String>,
    local_free: bool,
}
fn raw_connection(r: &Row<'_>) -> rusqlite::Result<RawConnection> {
    Ok(RawConnection {
        id: r.get(0)?,
        name: r.get(1)?,
        profile: r.get(2)?,
        endpoint: r.get(3)?,
        credential_ref: r.get(4)?,
        credential_revision: r.get(5)?,
        account_revision: r.get(26)?,
        credential_epoch: r.get(6)?,
        lifecycle: r.get(7)?,
        state_reason: r.get(8)?,
        account_digest: r.get(9)?,
        access_expires_at_ms: r.get(10)?,
        risk_notice: r.get(11)?,
        risk_binding: r.get(12)?,
        risk_epoch: r.get(13)?,
        risk_by: r.get(14)?,
        risk_at: r.get(15)?,
        discovered: r.get(16)?,
        discovered_at_ms: r.get(17)?,
        healthy: r.get(18)?,
        checked_at_ms: r.get(19)?,
        revision: r.get(20)?,
        created_at_ms: r.get(21)?,
        updated_at_ms: r.get(22)?,
        enabled: r.get(23)?,
        open_router: r.get(24)?,
        local_free: r.get(25)?,
    })
}
fn discovered_of(json: &str) -> Result<Vec<DiscoveredModel>, StorageError> {
    let v: Value = serde_json::from_str(json).map_err(|_| corrupt("stored discovery"))?;
    v.as_array()
        .ok_or_else(|| corrupt("stored discovery"))?
        .iter()
        .map(|m| {
            Ok(DiscoveredModel {
                id: m["id"]
                    .as_str()
                    .ok_or_else(|| corrupt("stored discovery"))?
                    .to_owned(),
                context_tokens: m["contextTokens"].as_i64(),
            })
        })
        .collect()
}
fn discovered_json(models: &[DiscoveredModel]) -> String {
    Value::Array(
        models
            .iter()
            .map(|m| match m.context_tokens {
                Some(c) => json!({"id": m.id, "contextTokens": c}),
                None => json!({"id": m.id}),
            })
            .collect(),
    )
    .to_string()
}
fn connection_of(r: RawConnection) -> Result<StoredConnection, StorageError> {
    let risk = match (
        r.risk_notice,
        r.risk_binding,
        r.risk_epoch,
        r.risk_by,
        r.risk_at,
    ) {
        (Some(notice_sha256), Some(binding), Some(restore_epoch), Some(accepted_by), Some(at)) => {
            Some(StoredRiskAcceptance {
                notice_sha256,
                binding,
                restore_epoch,
                accepted_by,
                accepted_at_ms: at,
            })
        }
        (None, None, None, None, None) => None,
        _ => return Err(corrupt("partial risk acceptance")),
    };
    Ok(StoredConnection {
        id: id_of(r.id)?,
        name: r.name,
        profile: r.profile,
        endpoint: r.endpoint,
        enabled: r.enabled,
        local_free: r.local_free,
        open_router: r.open_router,
        credential_ref: r.credential_ref.map(id_of).transpose()?,
        credential_revision: r.credential_revision,
        account_revision: r.account_revision,
        credential_epoch: r.credential_epoch,
        lifecycle: r.lifecycle,
        state_reason: r.state_reason,
        account_digest: r.account_digest,
        access_expires_at_ms: r.access_expires_at_ms,
        risk,
        discovered: r.discovered.as_deref().map(discovered_of).transpose()?,
        discovered_at_ms: r.discovered_at_ms,
        healthy: r.healthy,
        checked_at_ms: r.checked_at_ms,
        revision: r.revision,
        created_at_ms: r.created_at_ms,
        updated_at_ms: r.updated_at_ms,
    })
}

pub(super) fn connection(
    conn: &Connection,
    id: &Id,
) -> Result<Option<StoredConnection>, StorageError> {
    db(conn
        .query_row(
            &format!("SELECT {CONNECTION_COLUMNS} FROM router_connections WHERE id=?1"),
            params![id.as_str()],
            raw_connection,
        )
        .optional())?
    .map(connection_of)
    .transpose()
}

pub(super) fn connections(
    conn: &Connection,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<StoredConnection>, StorageError> {
    if limit == 0 || limit > 500 {
        return Err(invalid("limit must be 1..=500"));
    }
    let mut st = db(conn.prepare_cached(&format!(
        "SELECT {CONNECTION_COLUMNS} FROM router_connections WHERE id > ?1 ORDER BY id LIMIT ?2"
    )))?;
    let rows = db(st
        .query_map(
            params![after.unwrap_or(""), i64::from(limit)],
            raw_connection,
        )
        .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>()))?;
    rows.into_iter().map(connection_of).collect()
}

fn validate_connection(c: &StoredConnection) -> Result<(), StorageError> {
    if !PROFILES.contains(&c.profile.as_str()) {
        return Err(invalid("unknown endpoint profile"));
    }
    if !STORED_LIFECYCLES.contains(&c.lifecycle.as_str()) {
        return Err(invalid("a connection lifecycle that is never stored"));
    }
    if c.name.is_empty() || c.name.chars().count() > 256 {
        return Err(invalid("connection name must be 1..=256 characters"));
    }
    if !(c.endpoint.starts_with("https://") || c.endpoint.starts_with("http://")) {
        return Err(invalid("endpoint must be an http(s) URL"));
    }
    if c.credential_ref.is_some() != c.credential_epoch.is_some() {
        return Err(invalid("a credential reference needs its restore epoch"));
    }
    if c.risk.is_some() && c.profile != "ClaudeSubscriptionMessages" {
        return Err(invalid(
            "only a Claude subscription carries a risk acceptance",
        ));
    }
    if let Some(o) = &c.open_router {
        if c.profile != "OpenRouterChat" {
            return Err(invalid(
                "only an OpenRouter connection carries OpenRouter constraints",
            ));
        }
        if serde_json::from_str::<Value>(o)
            .ok()
            .is_none_or(|v| !v.is_object())
        {
            return Err(invalid("OpenRouter constraints must be a JSON object"));
        }
    }
    if c.account_revision < 1 {
        return Err(invalid("account revision must be positive"));
    }
    if c.credential_revision < 0 {
        return Err(invalid("credential revision must not be negative"));
    }
    Ok(())
}

pub(super) fn put_connection(
    conn: &Connection,
    c: &StoredConnection,
    expected: Option<i64>,
) -> Result<StoredConnection, StorageError> {
    validate_connection(c)?;
    let current = connection(conn, &c.id)?;
    let next = next_revision(
        "connection",
        &c.id,
        expected,
        current.as_ref().map(|c| c.revision),
    )?;
    if let Some(prev) = &current
        && prev.credential_revision > c.credential_revision
    {
        return Err(invalid("a credential revision never goes back"));
    }
    match &current {
        None if c.account_revision != 1 => {
            return Err(invalid("a new account identity starts at revision one"));
        }
        Some(prev)
            if c.account_revision != prev.account_revision
                && prev.account_revision.checked_add(1) != Some(c.account_revision) =>
        {
            return Err(invalid(
                "account revision must stay unchanged or advance by one",
            ));
        }
        _ => {}
    }
    if let Some(r) = &c.credential_ref {
        let shared: bool = db(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM router_connections WHERE credential_ref=?1 AND id<>?2)",
            params![r.as_str(), c.id.as_str()],
            |row| row.get(0),
        ))?;
        if shared {
            return Err(invalid("a credential handle belongs to one connection"));
        }
    }
    let risk = c.risk.as_ref();
    let created = current
        .as_ref()
        .map_or(c.created_at_ms, |p| p.created_at_ms);
    db(conn.execute(
        "INSERT INTO router_connections(id,name,profile,endpoint,credential_ref,credential_revision,credential_epoch,lifecycle,state_reason,account_digest,access_expires_at,risk_notice_sha256,risk_binding,risk_restore_epoch,risk_accepted_by,risk_accepted_at,discovered_json,discovered_at,healthy,checked_at,revision,created_at,updated_at,enabled,open_router_json,local_free,account_revision) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?25,?26,?27,?28) \
         ON CONFLICT(id) DO UPDATE SET name=excluded.name, profile=excluded.profile, endpoint=excluded.endpoint, \
         credential_ref=excluded.credential_ref, credential_revision=excluded.credential_revision, \
         credential_epoch=excluded.credential_epoch, lifecycle=excluded.lifecycle, state_reason=excluded.state_reason, \
         account_digest=excluded.account_digest, access_expires_at=excluded.access_expires_at, \
         risk_notice_sha256=excluded.risk_notice_sha256, risk_binding=excluded.risk_binding, \
         risk_restore_epoch=excluded.risk_restore_epoch, risk_accepted_by=excluded.risk_accepted_by, \
         risk_accepted_at=excluded.risk_accepted_at, discovered_json=excluded.discovered_json, \
         discovered_at=excluded.discovered_at, healthy=excluded.healthy, checked_at=excluded.checked_at, \
         revision=excluded.revision, updated_at=excluded.updated_at, enabled=excluded.enabled, \
         open_router_json=excluded.open_router_json, local_free=excluded.local_free, account_revision=excluded.account_revision WHERE revision=?24",
        params![
            c.id.as_str(),
            c.name,
            c.profile,
            c.endpoint,
            c.credential_ref.as_ref().map(Id::as_str),
            c.credential_revision,
            c.credential_epoch,
            c.lifecycle,
            c.state_reason,
            c.account_digest,
            c.access_expires_at_ms,
            risk.map(|r| r.notice_sha256.as_str()),
            risk.map(|r| r.binding.as_str()),
            risk.map(|r| r.restore_epoch),
            risk.map(|r| r.accepted_by.as_str()),
            risk.map(|r| r.accepted_at_ms),
            c.discovered.as_deref().map(discovered_json),
            c.discovered_at_ms,
            c.healthy,
            c.checked_at_ms,
            next,
            created,
            c.updated_at_ms,
            expected.unwrap_or(0),
            c.enabled,
            c.open_router,
            c.local_free,
            c.account_revision,
        ],
    ))?;
    connection(conn, &c.id)?.ok_or_else(|| corrupt("connection vanished inside its transaction"))
}

pub(super) fn delete_connection(
    conn: &Connection,
    id: &Id,
    expected: i64,
) -> Result<(), StorageError> {
    let Some(current) = connection(conn, id)? else {
        return Err(StorageError::NotFound {
            kind: "connection",
            id: id.as_str().to_owned(),
        });
    };
    if current.revision != expected {
        return Err(conflict("connection", id, expected, current.revision));
    }
    let bound: bool = db(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM router_models WHERE connection_id=?1) \
         OR EXISTS(SELECT 1 FROM router_decision_models WHERE connection_id=?1)",
        params![id.as_str()],
        |r| r.get(0),
    ))?;
    if bound {
        return Err(invalid("models are still bound to the connection"));
    }
    db(conn.execute(
        "DELETE FROM router_connections WHERE id=?1 AND revision=?2",
        params![id.as_str(), expected],
    ))?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Decision models

const DECISION_COLUMNS: &str = "id,display_name,connection_id,upstream_model_id,question_types_json,accepts_images,accepts_videos,enabled,extension_schemas_json,result_schema_json,revision,updated_at";

fn decision_of(r: &Row<'_>) -> rusqlite::Result<RawDecision> {
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
    ))
}
type RawDecision = (
    String,
    String,
    String,
    String,
    String,
    bool,
    bool,
    bool,
    String,
    String,
    i64,
    i64,
);
fn stored_decision(r: RawDecision) -> Result<StoredDecisionModel, StorageError> {
    Ok(StoredDecisionModel {
        id: id_of(r.0)?,
        display_name: r.1,
        connection_id: id_of(r.2)?,
        upstream_model_id: r.3,
        question_types: serde_json::from_str(&r.4).map_err(|_| corrupt("stored question types"))?,
        accepts_images: r.5,
        accepts_videos: r.6,
        enabled: r.7,
        extension_schemas: r.8,
        result_schema: r.9,
        revision: r.10,
        updated_at_ms: r.11,
    })
}

pub(super) fn decision_model(
    conn: &Connection,
    id: &Id,
) -> Result<Option<StoredDecisionModel>, StorageError> {
    db(conn
        .query_row(
            &format!("SELECT {DECISION_COLUMNS} FROM router_decision_models WHERE id=?1"),
            params![id.as_str()],
            decision_of,
        )
        .optional())?
    .map(stored_decision)
    .transpose()
}

pub(super) fn decision_models(
    conn: &Connection,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<StoredDecisionModel>, StorageError> {
    if limit == 0 || limit > 500 {
        return Err(invalid("limit must be 1..=500"));
    }
    let mut st = db(conn.prepare_cached(&format!(
        "SELECT {DECISION_COLUMNS} FROM router_decision_models WHERE id > ?1 ORDER BY id LIMIT ?2"
    )))?;
    let rows = db(st
        .query_map(params![after.unwrap_or(""), i64::from(limit)], decision_of)
        .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>()))?;
    rows.into_iter().map(stored_decision).collect()
}

pub(super) fn put_decision_model(
    conn: &Connection,
    m: &StoredDecisionModel,
    expected: Option<i64>,
) -> Result<StoredDecisionModel, StorageError> {
    if m.question_types.is_empty()
        || m.question_types
            .iter()
            .any(|q| !QUESTION_TYPES.contains(&q.as_str()))
    {
        return Err(invalid("decision models declare known question types"));
    }
    let unique: std::collections::BTreeSet<&String> = m.question_types.iter().collect();
    if unique.len() != m.question_types.len() {
        return Err(invalid("duplicate question type declared"));
    }
    let taken: bool = db(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM router_models WHERE id=?1)",
        params![m.id.as_str()],
        |r| r.get(0),
    ))?;
    if taken {
        return Err(invalid(
            "a conversational model's ID cannot also be a decision model",
        ));
    }
    match connection(conn, &m.connection_id)? {
        None => return Err(invalid("the decision model's connection does not exist")),
        Some(c) if c.profile != "SystemOne" => {
            return Err(invalid(
                "a decision model is served by a SystemOne connection",
            ));
        }
        Some(_) => {}
    }
    for (what, text) in [
        ("extension schemas", &m.extension_schemas),
        ("result schema", &m.result_schema),
    ] {
        serde_json::from_str::<Value>(text)
            .map_err(|_| invalid(format!("decision model {what}")))?;
    }
    let current = decision_model(conn, &m.id)?;
    let next = next_revision(
        "decision_model",
        &m.id,
        expected,
        current.map(|c| c.revision),
    )?;
    let types = serde_json::to_string(&m.question_types).map_err(|e| invalid(e.to_string()))?;
    db(conn.execute(
        "INSERT INTO router_decision_models(id,display_name,connection_id,upstream_model_id,question_types_json,accepts_images,accepts_videos,enabled,extension_schemas_json,result_schema_json,revision,updated_at) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12) \
         ON CONFLICT(id) DO UPDATE SET display_name=excluded.display_name, connection_id=excluded.connection_id, \
         upstream_model_id=excluded.upstream_model_id, question_types_json=excluded.question_types_json, \
         accepts_images=excluded.accepts_images, accepts_videos=excluded.accepts_videos, enabled=excluded.enabled, \
         extension_schemas_json=excluded.extension_schemas_json, result_schema_json=excluded.result_schema_json, \
         revision=excluded.revision, updated_at=excluded.updated_at WHERE revision=?13",
        params![
            m.id.as_str(),
            m.display_name,
            m.connection_id.as_str(),
            m.upstream_model_id,
            types,
            m.accepts_images,
            m.accepts_videos,
            m.enabled,
            m.extension_schemas,
            m.result_schema,
            next,
            m.updated_at_ms,
            expected.unwrap_or(0),
        ],
    ))?;
    decision_model(conn, &m.id)?
        .ok_or_else(|| corrupt("decision model vanished inside its transaction"))
}
