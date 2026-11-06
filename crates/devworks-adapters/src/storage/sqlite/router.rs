//! SQLite inference router records (migration 13): `router_models`, `router_tier_cursors`,
//! `router_pins`, `router_cooldowns` and `router_requests`.
use super::conn::db;
use super::connections::{connection, decision_model};
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{Id, OperationId, ProjectId, Revision, RevisionDimension};
use rusqlite::{Connection, OptionalExtension, Row, params};

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
fn strings(json: &str) -> Result<Vec<String>, StorageError> {
    serde_json::from_str(json).map_err(|_| corrupt("stored router list"))
}
fn revision(n: i64) -> Result<Revision, StorageError> {
    Revision::new(n).map_err(|_| corrupt("stored router revision"))
}
fn conflict(kind: &'static str, id: &Id, expected: i64, current: i64) -> StorageError {
    // A cursor at 0 has no positive revision of its own; report it as revision 1.
    StorageError::conflict(
        RevisionDimension::Configuration,
        kind,
        id.clone(),
        Revision::new(expected.max(1)).expect("positive"),
        Revision::new(current.max(1)).expect("positive"),
    )
}

const EFFORTS: [&str; 6] = ["Minimal", "Low", "Medium", "High", "XHigh", "Max"];
const TIERS: [&str; 4] = ["Max", "High", "Mid", "Low"];
const PURPOSES: [&str; 6] = [
    "Conversation",
    "Compaction",
    "Title",
    "Enrichment",
    "Embedding",
    "Vision",
];

// ---------------------------------------------------------------------------------------------
// Models

const MODEL_COLUMNS: &str = "id,display_name,endpoint_profile,upstream_model_id,enabled,efforts_json,image_input,tools,context_tokens,position,revision,updated_at,connection_id";

/// A stored catalog row before its identifiers and lists are validated.
struct RawModel {
    id: String,
    display_name: String,
    endpoint_profile: String,
    upstream_model_id: String,
    enabled: bool,
    efforts: String,
    image_input: bool,
    tools: bool,
    context_tokens: Option<i64>,
    position: i64,
    revision: i64,
    updated_at_ms: i64,
    connection_id: Option<String>,
}
fn raw_model(r: &Row<'_>) -> rusqlite::Result<RawModel> {
    Ok(RawModel {
        id: r.get(0)?,
        display_name: r.get(1)?,
        endpoint_profile: r.get(2)?,
        upstream_model_id: r.get(3)?,
        enabled: r.get(4)?,
        efforts: r.get(5)?,
        image_input: r.get(6)?,
        tools: r.get(7)?,
        context_tokens: r.get(8)?,
        position: r.get(9)?,
        revision: r.get(10)?,
        updated_at_ms: r.get(11)?,
        connection_id: r.get(12)?,
    })
}
fn model_of(r: RawModel) -> Result<StoredRouteModel, StorageError> {
    Ok(StoredRouteModel {
        id: id_of(r.id)?,
        display_name: r.display_name,
        endpoint_profile: r.endpoint_profile,
        upstream_model_id: r.upstream_model_id,
        enabled: r.enabled,
        efforts: strings(&r.efforts)?,
        image_input: r.image_input,
        tools: r.tools,
        context_tokens: r.context_tokens,
        connection_id: opt_id(r.connection_id)?,
        position: r.position,
        revision: r.revision,
        updated_at_ms: r.updated_at_ms,
    })
}
fn model(conn: &Connection, id: &Id) -> Result<Option<StoredRouteModel>, StorageError> {
    db(conn
        .query_row(
            &format!("SELECT {MODEL_COLUMNS} FROM router_models WHERE id=?1"),
            params![id.as_str()],
            raw_model,
        )
        .optional())?
    .map(model_of)
    .transpose()
}
fn models(
    conn: &Connection,
    after: Option<(i64, &str)>,
    limit: u16,
) -> Result<Vec<StoredRouteModel>, StorageError> {
    if limit == 0 || limit > 500 {
        return Err(invalid("limit must be 1..=500"));
    }
    let (pos, id) = after.map_or((i64::MIN, ""), |(p, i)| (p, i));
    let mut st = db(conn.prepare_cached(&format!(
        "SELECT {MODEL_COLUMNS} FROM router_models WHERE (position, id) > (?1, ?2) \
         ORDER BY position, id LIMIT ?3"
    )))?;
    let rows = db(st
        .query_map(params![pos, id, i64::from(limit)], raw_model)
        .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>()))?;
    rows.into_iter().map(model_of).collect()
}
fn validate_model(m: &StoredRouteModel) -> Result<(), StorageError> {
    if m.endpoint_profile == "SystemOne" {
        return Err(invalid("a decision model cannot be a conversational model"));
    }
    if m.efforts.iter().any(|e| !EFFORTS.contains(&e.as_str())) {
        return Err(invalid("unknown or default effort declared"));
    }
    let unique: std::collections::BTreeSet<&String> = m.efforts.iter().collect();
    if unique.len() != m.efforts.len() {
        return Err(invalid("duplicate effort declared"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Pins

const PIN_COLUMNS: &str = "session_id,pin_id,revision,model_id,connection_id,intent_json,requested_effort,effective_effort,capabilities_json,replaced_pin_id,reason,updated_at,upstream_model_id,endpoint_profile,identity_origin";

type RawPin = (
    String,
    String,
    i64,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    i64,
    Option<String>,
    Option<String>,
    String,
);
fn raw_pin(r: &Row<'_>) -> rusqlite::Result<RawPin> {
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
fn pin_of(r: RawPin) -> Result<StoredPin, StorageError> {
    Ok(StoredPin {
        session_id: id_of(r.0)?,
        pin_id: id_of(r.1)?,
        revision: r.2,
        model_id: id_of(r.3)?,
        connection_id: id_of(r.4)?,
        intent: r.5,
        requested_effort: r.6,
        effective_effort: r.7,
        required_capabilities: strings(&r.8)?,
        replaced_pin_id: opt_id(r.9)?,
        reason: r.10,
        updated_at_ms: r.11,
        upstream_model_id: r.12,
        endpoint_profile: r.13,
        identity_origin: r.14,
    })
}
fn pin(conn: &Connection, session: &Id) -> Result<Option<StoredPin>, StorageError> {
    db(conn
        .query_row(
            &format!("SELECT {PIN_COLUMNS} FROM router_pins WHERE session_id=?1"),
            params![session.as_str()],
            raw_pin,
        )
        .optional())?
    .map(pin_of)
    .transpose()
}
fn validate_pin(p: &StoredPin) -> Result<(), StorageError> {
    let identity_known = p
        .upstream_model_id
        .as_ref()
        .is_some_and(|s| !s.is_empty() && s.len() <= 4096)
        && p.endpoint_profile.as_ref().is_some_and(|s| !s.is_empty());
    if !matches!(
        p.identity_origin.as_str(),
        "Unknown" | "Resolved" | "Migration25CurrentCatalog"
    ) || (p.identity_origin == "Unknown"
        && (p.upstream_model_id.is_some() || p.endpoint_profile.is_some()))
        || (p.identity_origin != "Unknown" && !identity_known)
    {
        return Err(invalid("invalid concrete pin identity snapshot"));
    }
    if p.requested_effort != "Default" && p.requested_effort != p.effective_effort {
        return Err(invalid("an explicit effort cannot change"));
    }
    if !PIN_REASONS.contains(&p.reason.as_str()) {
        return Err(invalid("unknown pin reason"));
    }
    if serde_json::from_str::<serde_json::Value>(&p.intent)
        .ok()
        .is_none_or(|v| !v.is_object())
    {
        return Err(invalid("route intent must be a JSON object"));
    }
    let mut caps = p.required_capabilities.clone();
    caps.sort();
    caps.dedup();
    if caps != p.required_capabilities {
        return Err(invalid("capabilities must be sorted and unique"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Cooldowns and requests

fn cooldowns(conn: &Connection) -> Result<Vec<StoredCooldown>, StorageError> {
    let mut st = db(conn.prepare_cached(
        "SELECT model_id,until_at,reason,observed_at FROM router_cooldowns ORDER BY model_id",
    ))?;
    let rows = db(st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })
        .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>()))?;
    rows.into_iter()
        .map(|(m, until_ms, reason, observed_at_ms)| {
            Ok(StoredCooldown {
                model_id: id_of(m)?,
                until_ms,
                reason,
                observed_at_ms,
            })
        })
        .collect()
}
fn cooldown(conn: &Connection, model: &Id) -> Result<Option<StoredCooldown>, StorageError> {
    Ok(cooldowns(conn)?.into_iter().find(|c| &c.model_id == model))
}

type RawRequest = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
    i64,
);
fn request(conn: &Connection, id: &Id) -> Result<Option<StoredRouteRequest>, StorageError> {
    let raw: Option<RawRequest> = db(conn
        .query_row(
            "SELECT request_id,operation_id,purpose,session_id,project_id,model_id,connection_id,pin_id,created_at FROM router_requests WHERE request_id=?1",
            params![id.as_str()],
            |r| {
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
            },
        )
        .optional())?;
    raw.map(|r| {
        Ok(StoredRouteRequest {
            request_id: id_of(r.0)?,
            operation_id: OperationId(id_of(r.1)?),
            purpose: r.2,
            session_id: opt_id(r.3)?,
            project_id: opt_id(r.4)?.map(ProjectId),
            model_id: id_of(r.5)?,
            connection_id: id_of(r.6)?,
            pin_id: opt_id(r.7)?,
            created_at_ms: r.8,
        })
    })
    .transpose()
}

macro_rules! router_reads {
    ($t:ty) => {
        impl RouterRead for $t {
            fn route_model(&self, id: &Id) -> Result<Option<StoredRouteModel>, StorageError> {
                model(self.conn, id)
            }
            fn connection(&self, id: &Id) -> Result<Option<StoredConnection>, StorageError> {
                connection(self.conn, id)
            }
            fn connections(
                &self,
                after: Option<&str>,
                limit: u16,
            ) -> Result<Vec<StoredConnection>, StorageError> {
                super::connections::connections(self.conn, after, limit)
            }
            fn decision_model(&self, id: &Id) -> Result<Option<StoredDecisionModel>, StorageError> {
                decision_model(self.conn, id)
            }
            fn decision_models(
                &self,
                after: Option<&str>,
                limit: u16,
            ) -> Result<Vec<StoredDecisionModel>, StorageError> {
                super::connections::decision_models(self.conn, after, limit)
            }
            fn route_models(
                &self,
                after: Option<(i64, &str)>,
                limit: u16,
            ) -> Result<Vec<StoredRouteModel>, StorageError> {
                models(self.conn, after, limit)
            }
            fn tier_cursor(&self, tier: &str) -> Result<i64, StorageError> {
                if !TIERS.contains(&tier) {
                    return Err(invalid(format!("unknown tier {tier}")));
                }
                Ok(db(self
                    .conn
                    .query_row(
                        "SELECT next FROM router_tier_cursors WHERE tier=?1",
                        params![tier],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional())?
                .unwrap_or(0))
            }
            fn session_pin(&self, session: &Id) -> Result<Option<StoredPin>, StorageError> {
                pin(self.conn, session)
            }
            fn cooldowns(&self) -> Result<Vec<StoredCooldown>, StorageError> {
                cooldowns(self.conn)
            }
            fn route_request(&self, id: &Id) -> Result<Option<StoredRouteRequest>, StorageError> {
                request(self.conn, id)
            }
        }
    };
}
router_reads!(Snapshot<'_>);
router_reads!(Unit<'_>);

impl RouterRepository for Unit<'_> {
    fn put_route_model(
        &mut self,
        m: &StoredRouteModel,
        expected_revision: Option<i64>,
    ) -> Result<StoredRouteModel, StorageError> {
        validate_model(m)?;
        let efforts = serde_json::to_string(&m.efforts).map_err(|e| invalid(e.to_string()))?;
        let current = model(self.conn, &m.id)?;
        let next = match (expected_revision, &current) {
            (None, None) => 1,
            (None, Some(_)) => {
                return Err(StorageError::Duplicate {
                    kind: "route_model",
                    id: m.id.as_str().to_owned(),
                });
            }
            (Some(_), None) => {
                return Err(StorageError::NotFound {
                    kind: "route_model",
                    id: m.id.as_str().to_owned(),
                });
            }
            (Some(e), Some(c)) if e != c.revision => {
                return Err(conflict("route_model", &m.id, e, c.revision));
            }
            (Some(e), Some(_)) => e + 1,
        };
        if decision_model(self.conn, &m.id)?.is_some() {
            return Err(invalid(
                "a decision model's ID cannot also be a conversational model",
            ));
        }
        if let Some(c) = &m.connection_id {
            match connection(self.conn, c)? {
                None => return Err(invalid("the model's connection does not exist")),
                Some(conn) if conn.profile != m.endpoint_profile => {
                    return Err(invalid(
                        "the model's connection serves another endpoint profile",
                    ));
                }
                Some(_) => {}
            }
        }
        db(self.conn.execute(
            "INSERT INTO router_models(id,display_name,endpoint_profile,upstream_model_id,enabled,efforts_json,image_input,tools,context_tokens,position,revision,updated_at,connection_id) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13) \
             ON CONFLICT(id) DO UPDATE SET display_name=excluded.display_name, endpoint_profile=excluded.endpoint_profile, \
             upstream_model_id=excluded.upstream_model_id, enabled=excluded.enabled, efforts_json=excluded.efforts_json, \
             image_input=excluded.image_input, tools=excluded.tools, context_tokens=excluded.context_tokens, \
             position=excluded.position, revision=excluded.revision, updated_at=excluded.updated_at, \
             connection_id=excluded.connection_id",
            params![
                m.id.as_str(),
                m.display_name,
                m.endpoint_profile,
                m.upstream_model_id,
                m.enabled,
                efforts,
                m.image_input,
                m.tools,
                m.context_tokens,
                m.position,
                next,
                m.updated_at_ms,
                m.connection_id.as_ref().map(Id::as_str),
            ],
        ))?;
        model(self.conn, &m.id)?.ok_or_else(|| corrupt("model vanished inside its transaction"))
    }

    fn advance_tier_cursor(&mut self, tier: &str, expected: i64) -> Result<i64, StorageError> {
        let current = self.tier_cursor(tier)?;
        if current != expected {
            let id = Id::new(tier).map_err(|e| invalid(e.to_string()))?;
            return Err(conflict("tier_cursor", &id, expected, current));
        }
        let next = expected
            .checked_add(1)
            .ok_or_else(|| invalid("tier cursor overflow"))?;
        db(self.conn.execute(
            "INSERT INTO router_tier_cursors(tier,next) VALUES(?1,?2) \
             ON CONFLICT(tier) DO UPDATE SET next=excluded.next WHERE next=?3",
            params![tier, next, expected],
        ))?;
        if self.tier_cursor(tier)? != next {
            return Err(corrupt("tier cursor changed inside its transaction"));
        }
        Ok(next)
    }

    fn put_session_pin(
        &mut self,
        p: &StoredPin,
        expected_revision: Option<i64>,
    ) -> Result<StoredPin, StorageError> {
        validate_pin(p)?;
        let caps =
            serde_json::to_string(&p.required_capabilities).map_err(|e| invalid(e.to_string()))?;
        let current = pin(self.conn, &p.session_id)?;
        let next = match (expected_revision, &current) {
            (None, None) => 1,
            (None, Some(_)) => {
                return Err(StorageError::Duplicate {
                    kind: "route_pin",
                    id: p.session_id.as_str().to_owned(),
                });
            }
            (Some(_), None) => {
                return Err(StorageError::NotFound {
                    kind: "route_pin",
                    id: p.session_id.as_str().to_owned(),
                });
            }
            (Some(e), Some(c)) if e != c.revision => {
                return Err(conflict("route_pin", &p.session_id, e, c.revision));
            }
            (Some(e), Some(_)) => e + 1,
        };
        revision(next)?;
        let n = db(self.conn.execute(
            "INSERT INTO router_pins(session_id,pin_id,revision,model_id,connection_id,intent_json,requested_effort,effective_effort,capabilities_json,replaced_pin_id,reason,updated_at,upstream_model_id,endpoint_profile,identity_origin) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15) \
             ON CONFLICT(session_id) DO UPDATE SET pin_id=excluded.pin_id, revision=excluded.revision, \
             model_id=excluded.model_id, connection_id=excluded.connection_id, intent_json=excluded.intent_json, \
             requested_effort=excluded.requested_effort, effective_effort=excluded.effective_effort, \
             capabilities_json=excluded.capabilities_json, replaced_pin_id=excluded.replaced_pin_id, \
             reason=excluded.reason, updated_at=excluded.updated_at, \
             upstream_model_id=excluded.upstream_model_id, endpoint_profile=excluded.endpoint_profile, \
             identity_origin=excluded.identity_origin WHERE revision=?16",
            params![
                p.session_id.as_str(),
                p.pin_id.as_str(),
                next,
                p.model_id.as_str(),
                p.connection_id.as_str(),
                p.intent,
                p.requested_effort,
                p.effective_effort,
                caps,
                p.replaced_pin_id.as_ref().map(Id::as_str),
                p.reason,
                p.updated_at_ms,
                p.upstream_model_id,
                p.endpoint_profile,
                p.identity_origin,
                expected_revision.unwrap_or(0),
            ],
        ))?;
        if n != 1 {
            return Err(corrupt("pin changed inside its transaction"));
        }
        pin(self.conn, &p.session_id)?.ok_or_else(|| corrupt("pin vanished inside its transaction"))
    }

    fn record_cooldown(&mut self, c: &StoredCooldown) -> Result<StoredCooldown, StorageError> {
        if !COOLDOWN_REASONS.contains(&c.reason.as_str()) {
            return Err(invalid("unknown cooldown reason"));
        }
        // The later expiry wins: a shorter report never shortens a running cooldown.
        db(self.conn.execute(
            "INSERT INTO router_cooldowns(model_id,until_at,reason,observed_at) VALUES(?1,?2,?3,?4) \
             ON CONFLICT(model_id) DO UPDATE SET until_at=excluded.until_at, reason=excluded.reason, \
             observed_at=excluded.observed_at WHERE excluded.until_at > router_cooldowns.until_at",
            params![c.model_id.as_str(), c.until_ms, c.reason, c.observed_at_ms],
        ))?;
        cooldown(self.conn, &c.model_id)?
            .ok_or_else(|| corrupt("cooldown vanished inside its transaction"))
    }

    fn clear_cooldown(&mut self, model: &Id) -> Result<bool, StorageError> {
        Ok(db(self.conn.execute(
            "DELETE FROM router_cooldowns WHERE model_id=?1",
            params![model.as_str()],
        ))? == 1)
    }

    fn record_route_request(&mut self, r: &StoredRouteRequest) -> Result<(), StorageError> {
        if !PURPOSES.contains(&r.purpose.as_str()) {
            return Err(invalid("unknown request purpose"));
        }
        if request(self.conn, &r.request_id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "route_request",
                id: r.request_id.as_str().to_owned(),
            });
        }
        let taken: bool = db(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM router_requests WHERE operation_id=?1)",
            params![r.operation_id.0.as_str()],
            |row| row.get(0),
        ))?;
        if taken {
            return Err(StorageError::Duplicate {
                kind: "route_request_operation",
                id: r.operation_id.0.as_str().to_owned(),
            });
        }
        db(self.conn.execute(
            "INSERT INTO router_requests(request_id,operation_id,purpose,session_id,project_id,model_id,connection_id,pin_id,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                r.request_id.as_str(),
                r.operation_id.0.as_str(),
                r.purpose,
                r.session_id.as_ref().map(Id::as_str),
                r.project_id.as_ref().map(|p| p.0.as_str()),
                r.model_id.as_str(),
                r.connection_id.as_str(),
                r.pin_id.as_ref().map(Id::as_str),
                r.created_at_ms,
            ],
        ))?;
        Ok(())
    }

    fn put_connection(
        &mut self,
        c: &StoredConnection,
        expected_revision: Option<i64>,
    ) -> Result<StoredConnection, StorageError> {
        super::connections::put_connection(self.conn, c, expected_revision)
    }

    fn delete_connection(&mut self, id: &Id, expected_revision: i64) -> Result<(), StorageError> {
        super::connections::delete_connection(self.conn, id, expected_revision)
    }

    fn put_decision_model(
        &mut self,
        m: &StoredDecisionModel,
        expected_revision: Option<i64>,
    ) -> Result<StoredDecisionModel, StorageError> {
        super::connections::put_decision_model(self.conn, m, expected_revision)
    }
}
