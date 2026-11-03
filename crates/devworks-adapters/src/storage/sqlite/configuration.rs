//! SQLite configuration repository: `configuration_revisions`/`effective_configuration` (complete
//! revision chain per scope and namespace) and `configuration_proposals` (migration 4).
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, Revision, RevisionDimension};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

fn invalid(m: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(m.into())
}
fn corrupt(m: impl Into<String>) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, m)
}
fn json_of(s: &str) -> Result<Value, StorageError> {
    serde_json::from_str(s).map_err(|e| corrupt(format!("stored configuration JSON: {e}")))
}
fn id_of(s: String) -> Result<Id, StorageError> {
    Id::new(s).map_err(|e| corrupt(format!("stored identifier: {e}")))
}
fn scope_of(key: &str) -> Result<ConfigScope, StorageError> {
    ConfigScope::from_key(key).ok_or_else(|| corrupt(format!("stored scope key {key}")))
}
fn namespace_ok(ns: &str) -> Result<(), StorageError> {
    if ns.is_empty() || ns.len() > 128 || ns.contains('\0') {
        return Err(invalid("configuration namespace must be 1..=128 bytes"));
    }
    Ok(())
}
fn limit(l: u16) -> Result<i64, StorageError> {
    if l == 0 || l > MAX_PAGE {
        return Err(invalid(format!("limit must be 1..={MAX_PAGE}")));
    }
    Ok(i64::from(l))
}
fn rev(v: i64) -> Result<Revision, StorageError> {
    Revision::new(v).map_err(|_| invalid("revision must be positive"))
}
fn state_name(s: ProposalState) -> &'static str {
    match s {
        ProposalState::Pending => "Pending",
        ProposalState::Approved => "Approved",
        ProposalState::Rejected => "Rejected",
    }
}
fn parse_state(s: &str) -> Result<ProposalState, StorageError> {
    Ok(match s {
        "Pending" => ProposalState::Pending,
        "Approved" => ProposalState::Approved,
        "Rejected" => ProposalState::Rejected,
        other => return Err(corrupt(format!("unknown proposal state {other}"))),
    })
}

fn scope_exists(conn: &Connection, scope: &ConfigScope) -> Result<bool, StorageError> {
    match scope {
        ConfigScope::Workspace => Ok(true),
        ConfigScope::Project(p) => db(conn
            .prepare_cached("SELECT 1 FROM projects WHERE id=?1")
            .and_then(|mut s| s.exists([p.0.as_str()]))),
    }
}

fn revision_row(r: &Row<'_>) -> rusqlite::Result<(String, String, i64, i64, String)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
}
fn revision_of(raw: (String, String, i64, i64, String)) -> Result<ConfigRevision, StorageError> {
    let (scope, namespace, revision, schema_version, body) = raw;
    Ok(ConfigRevision {
        scope: scope_of(&scope)?,
        namespace,
        revision,
        schema_version,
        body: json_of(&body)?,
    })
}
const REVISION_COLUMNS: &str = "r.scope_key,r.namespace,r.revision,r.schema_version,r.body_json";

fn current(
    conn: &Connection,
    scope: &ConfigScope,
    ns: &str,
) -> Result<Option<ConfigRevision>, StorageError> {
    let raw = db(conn
        .prepare_cached(&format!(
            "SELECT {REVISION_COLUMNS} FROM effective_configuration e JOIN configuration_revisions r USING(scope_key,namespace,revision) WHERE e.scope_key=?1 AND e.namespace=?2"
        ))
        .and_then(|mut s| s.query_row(params![scope.key(), ns], revision_row).optional()))?;
    raw.map(revision_of).transpose()
}

fn revision(
    conn: &Connection,
    scope: &ConfigScope,
    ns: &str,
    revision: i64,
) -> Result<Option<ConfigRevision>, StorageError> {
    let raw = db(conn
        .prepare_cached(&format!(
            "SELECT {REVISION_COLUMNS} FROM configuration_revisions r WHERE r.scope_key=?1 AND r.namespace=?2 AND r.revision=?3"
        ))
        .and_then(|mut s| {
            s.query_row(params![scope.key(), ns, revision], revision_row)
                .optional()
        }))?;
    raw.map(revision_of).transpose()
}

fn history(
    conn: &Connection,
    scope: &ConfigScope,
    ns: &str,
    before: Option<i64>,
    max: u16,
) -> Result<Vec<ConfigRevision>, StorageError> {
    let n = limit(max)?;
    let mut stmt = db(conn.prepare_cached(&format!(
        "SELECT {REVISION_COLUMNS} FROM configuration_revisions r WHERE r.scope_key=?1 AND r.namespace=?2 AND r.revision<?3 ORDER BY r.revision DESC LIMIT ?4"
    )))?;
    let rows = db(stmt.query_map(
        params![scope.key(), ns, before.unwrap_or(i64::MAX), n],
        revision_row,
    ))?;
    let raw: Vec<_> = db(rows.collect())?;
    raw.into_iter().map(revision_of).collect()
}

const PROPOSAL_COLUMNS: &str = "id,scope_key,namespace,base_revision,revision,state,author_id,created_at,decided_by,decided_at,applied_revision,body_json";
type ProposalRaw = (
    String,
    String,
    String,
    i64,
    i64,
    String,
    String,
    i64,
    Option<String>,
    Option<i64>,
    Option<i64>,
    String,
);
fn proposal_row(r: &Row<'_>) -> rusqlite::Result<ProposalRaw> {
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
fn proposal_of(raw: ProposalRaw) -> Result<ProposalRecord, StorageError> {
    let (id, scope, namespace, base, revision, state, author, created, by, at, applied, body) = raw;
    Ok(ProposalRecord {
        id: id_of(id)?,
        scope: scope_of(&scope)?,
        namespace,
        base_revision: base,
        revision,
        state: parse_state(&state)?,
        author: ActorId(id_of(author)?),
        created_at_ms: created,
        decided_by: by.map(id_of).transpose()?.map(ActorId),
        decided_at_ms: at,
        applied_revision: applied,
        body: json_of(&body)?,
    })
}
fn proposal(conn: &Connection, id: &Id) -> Result<Option<ProposalRecord>, StorageError> {
    let raw = db(conn
        .prepare_cached(&format!(
            "SELECT {PROPOSAL_COLUMNS} FROM configuration_proposals WHERE id=?1"
        ))
        .and_then(|mut s| s.query_row([id.as_str()], proposal_row).optional()))?;
    raw.map(proposal_of).transpose()
}
fn proposals(
    conn: &Connection,
    scope: &ConfigScope,
    ns: &str,
    state: Option<ProposalState>,
    after: Option<&Id>,
    max: u16,
) -> Result<Vec<ProposalRecord>, StorageError> {
    let n = limit(max)?;
    let mut stmt = db(conn.prepare_cached(&format!(
        "SELECT {PROPOSAL_COLUMNS} FROM configuration_proposals WHERE scope_key=?1 AND namespace=?2 AND (?3 IS NULL OR state=?3) AND id>?4 ORDER BY id LIMIT ?5"
    )))?;
    let rows = db(stmt.query_map(
        params![
            scope.key(),
            ns,
            state.map(state_name),
            after.map_or("", Id::as_str),
            n
        ],
        proposal_row,
    ))?;
    let raw: Vec<_> = db(rows.collect())?;
    raw.into_iter().map(proposal_of).collect()
}

macro_rules! configuration_read {
    ($t:ty) => {
        impl ConfigurationRead for $t {
            fn scope_exists(&self, scope: &ConfigScope) -> Result<bool, StorageError> {
                scope_exists(self.conn, scope)
            }
            fn current(
                &self,
                scope: &ConfigScope,
                namespace: &str,
            ) -> Result<Option<ConfigRevision>, StorageError> {
                current(self.conn, scope, namespace)
            }
            fn revision(
                &self,
                scope: &ConfigScope,
                namespace: &str,
                number: i64,
            ) -> Result<Option<ConfigRevision>, StorageError> {
                revision(self.conn, scope, namespace, number)
            }
            fn history(
                &self,
                scope: &ConfigScope,
                namespace: &str,
                before: Option<i64>,
                limit: u16,
            ) -> Result<Vec<ConfigRevision>, StorageError> {
                history(self.conn, scope, namespace, before, limit)
            }
            fn proposal(&self, id: &Id) -> Result<Option<ProposalRecord>, StorageError> {
                proposal(self.conn, id)
            }
            fn proposals(
                &self,
                scope: &ConfigScope,
                namespace: &str,
                state: Option<ProposalState>,
                after: Option<&Id>,
                limit: u16,
            ) -> Result<Vec<ProposalRecord>, StorageError> {
                proposals(self.conn, scope, namespace, state, after, limit)
            }
        }
    };
}
configuration_read!(Snapshot<'_>);
configuration_read!(Unit<'_>);

fn require_scope(conn: &Connection, scope: &ConfigScope) -> Result<(), StorageError> {
    if scope_exists(conn, scope)? {
        return Ok(());
    }
    Err(StorageError::NotFound {
        kind: "project",
        id: scope.target_id().as_str().to_owned(),
    })
}
fn require_actor(conn: &Connection, actor: &ActorId) -> Result<(), StorageError> {
    let found = db(conn
        .prepare_cached("SELECT 1 FROM actors WHERE id=?1")
        .and_then(|mut s| s.exists([actor.0.as_str()])))?;
    if found {
        return Ok(());
    }
    Err(StorageError::NotFound {
        kind: "actor",
        id: actor.0.as_str().to_owned(),
    })
}

impl ConfigurationRepository for Unit<'_> {
    fn append_revision(
        &mut self,
        scope: &ConfigScope,
        ns: &str,
        expected: i64,
        schema_version: i64,
        body: Value,
    ) -> Result<ConfigRevision, StorageError> {
        namespace_ok(ns)?;
        if !body.is_object() {
            return Err(invalid("configuration body must be a JSON object"));
        }
        if schema_version <= 0 {
            return Err(invalid("configuration schema version must be positive"));
        }
        require_scope(self.conn, scope)?;
        let now = current(self.conn, scope, ns)?.map_or(1, |r| r.revision);
        if now != expected {
            return Err(StorageError::conflict(
                RevisionDimension::Configuration,
                "configuration_scope",
                scope.target_id(),
                rev(expected)?,
                rev(now)?,
            ));
        }
        let next = now
            .checked_add(1)
            .ok_or_else(|| invalid("configuration revision exhausted"))?;
        let key = scope.key();
        db(self.conn.execute(
            "INSERT INTO configuration_revisions(scope_key,namespace,revision,schema_version,body_json) VALUES(?1,?2,?3,?4,?5)",
            params![key, ns, next, schema_version, body.to_string()],
        ))?;
        db(self.conn.execute(
            "INSERT INTO effective_configuration(scope_key,namespace,revision) VALUES(?1,?2,?3) ON CONFLICT(scope_key,namespace) DO UPDATE SET revision=excluded.revision",
            params![key, ns, next],
        ))?;
        Ok(ConfigRevision {
            scope: scope.clone(),
            namespace: ns.to_owned(),
            revision: next,
            schema_version,
            body,
        })
    }

    fn stage(&mut self, p: NewProposal) -> Result<ProposalRecord, StorageError> {
        namespace_ok(&p.namespace)?;
        if !p.body.is_object() {
            return Err(invalid("proposal body must be a JSON object"));
        }
        rev(p.base_revision)?;
        require_scope(self.conn, &p.scope)?;
        require_actor(self.conn, &p.author)?;
        if proposal(self.conn, &p.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "configuration_proposal",
                id: p.id.as_str().to_owned(),
            });
        }
        db(self.conn.execute(
            "INSERT INTO configuration_proposals(id,scope_key,namespace,base_revision,revision,state,author_id,created_at,body_json) VALUES(?1,?2,?3,?4,1,'Pending',?5,?6,?7)",
            params![
                p.id.as_str(),
                p.scope.key(),
                p.namespace,
                p.base_revision,
                p.author.0.as_str(),
                self.now,
                p.body.to_string()
            ],
        ))?;
        proposal(self.conn, &p.id)?
            .ok_or_else(|| corrupt("proposal vanished inside its transaction"))
    }

    fn update_proposal(
        &mut self,
        id: &Id,
        expected_revision: i64,
        change: ProposalChange,
    ) -> Result<ProposalRecord, StorageError> {
        let p = proposal(self.conn, id)?.ok_or_else(|| StorageError::NotFound {
            kind: "configuration_proposal",
            id: id.as_str().to_owned(),
        })?;
        if p.revision != expected_revision {
            return Err(StorageError::conflict(
                RevisionDimension::Configuration,
                "configuration_proposal",
                id.clone(),
                rev(expected_revision)?,
                rev(p.revision)?,
            ));
        }
        if p.state != ProposalState::Pending {
            return Err(invalid("only a pending proposal can change"));
        }
        let n = match change {
            ProposalChange::Reconcile {
                base_revision,
                body,
            } => {
                rev(base_revision)?;
                if !body.is_object() {
                    return Err(invalid("proposal body must be a JSON object"));
                }
                db(self.conn.execute(
                    "UPDATE configuration_proposals SET base_revision=?1,body_json=?2,revision=revision+1 WHERE id=?3 AND revision=?4",
                    params![base_revision, body.to_string(), id.as_str(), p.revision],
                ))?
            }
            ProposalChange::Decide {
                state,
                decided_by,
                applied_revision,
            } => {
                match (state, applied_revision) {
                    (ProposalState::Approved, Some(_)) | (ProposalState::Rejected, None) => {}
                    _ => return Err(invalid("illegal proposal decision")),
                }
                require_actor(self.conn, &decided_by)?;
                db(self.conn.execute(
                    "UPDATE configuration_proposals SET state=?1,decided_by=?2,decided_at=?3,applied_revision=?4,revision=revision+1 WHERE id=?5 AND revision=?6",
                    params![
                        state_name(state),
                        decided_by.0.as_str(),
                        self.now,
                        applied_revision,
                        id.as_str(),
                        p.revision
                    ],
                ))?
            }
        };
        if n != 1 {
            return Err(corrupt("proposal changed inside its transaction"));
        }
        proposal(self.conn, id)?.ok_or_else(|| corrupt("proposal vanished inside its transaction"))
    }
}
