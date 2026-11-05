//! SQLite owned browser session records and their observation index: `browser_sessions` and
//! `browser_observations` (migration 16).
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, AssignmentId, Id, OperationId, ProjectId, RootId};
use rusqlite::{Connection, Row, params};

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
fn limit(l: u16) -> Result<i64, StorageError> {
    if l == 0 || l > MAX_PAGE {
        return Err(invalid(format!("limit must be 1..={MAX_PAGE}")));
    }
    Ok(i64::from(l))
}
fn lost(id: &Id) -> StorageError {
    StorageError::LeaseLost {
        kind: "browser_session",
        id: id.as_str().to_owned(),
    }
}

/// Owner kinds a session may record (`PrincipalKind` names).
const OWNER_KINDS: [&str; 6] = ["Human", "Agent", "Package", "McpClient", "Schedule", "Host"];
/// Most registered targets one session keeps (the browser service's live bound).
pub use devworks_core::ports::browser::MAX_TARGETS;

const COLUMNS: &str = "id,boot_id,state,owner_id,owner_kind,agent_session_id,project_id,root_id,assignment_id,asset_id,process_id,create_operation_id,uncertain_operation_id,targets,detail,last_action_operation_id,close_operation_id,tested_repository,source_binding,observation_seq,created_at,updated_at,revision";
type Raw = (Vec<Option<String>>, [i64; 4]);
fn row(r: &Row<'_>) -> rusqlite::Result<Raw> {
    let text = (0..19)
        .map(|i| r.get(i))
        .collect::<rusqlite::Result<Vec<Option<String>>>>()?;
    Ok((text, [r.get(19)?, r.get(20)?, r.get(21)?, r.get(22)?]))
}
fn session_of((text, num): Raw) -> Result<StoredBrowserSession, StorageError> {
    let mut t = text.into_iter();
    let mut next = || t.next().flatten();
    let need = |v: Option<String>| v.ok_or_else(|| corrupt("browser session column is NULL"));
    let id = id_of(need(next())?)?;
    let boot_id = id_of(need(next())?)?;
    let state = need(next())?;
    let phase = BrowserPhase::parse(&state)
        .ok_or_else(|| corrupt(format!("stored browser session state {state}")))?;
    let owner = ActorId(id_of(need(next())?)?);
    let owner_kind = need(next())?;
    let agent_session_id = opt_id(next())?;
    let project_id = opt_id(next())?.map(ProjectId);
    let root_id = opt_id(next())?.map(RootId);
    let assignment_id = opt_id(next())?.map(AssignmentId);
    let asset_id = need(next())?;
    let process_id = opt_id(next())?;
    let create_operation_id = OperationId(id_of(need(next())?)?);
    let uncertain_operation_id = opt_id(next())?.map(OperationId);
    let targets: Vec<String> = serde_json::from_str(&need(next())?)
        .map_err(|e| corrupt(format!("stored browser targets: {e}")))?;
    let detail = next();
    let last_action_operation_id = opt_id(next())?.map(OperationId);
    let close_operation_id = opt_id(next())?.map(OperationId);
    let tested_repository = next()
        .map(|v| serde_json::from_str(&v).map_err(|_| corrupt("browser source")))
        .transpose()?;
    validate_source(tested_repository.as_ref())?;
    let source_binding = next()
        .map(|v| serde_json::from_str(&v).map_err(|_| corrupt("browser source binding")))
        .transpose()?;
    Ok(StoredBrowserSession {
        id,
        revision: num[3],
        phase,
        boot_id,
        owner,
        owner_kind,
        agent_session_id,
        project_id,
        root_id,
        assignment_id,
        asset_id,
        tested_repository,
        source_binding,
        process_id,
        create_operation_id,
        uncertain_operation_id,
        last_action_operation_id,
        close_operation_id,
        targets,
        observation_sequence: num[0],
        detail,
        created_at_ms: num[1],
        updated_at_ms: num[2],
    })
}
fn sessions<P: rusqlite::Params>(
    conn: &Connection,
    tail: &str,
    p: P,
) -> Result<Vec<StoredBrowserSession>, StorageError> {
    let mut s = db(conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM browser_sessions WHERE {tail}"
    )))?;
    db(db(s.query_map(p, row))?.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(session_of)
        .collect()
}

fn observations(
    conn: &Connection,
    session: &Id,
    channel: BrowserChannel,
    after: i64,
    n: i64,
) -> Result<Vec<StoredBrowserObservation>, StorageError> {
    let mut s = db(conn.prepare_cached(
        "SELECT sequence,artifact_id,version_id,truncated,recorded_at,tested_repository FROM browser_observations WHERE session_id=?1 AND channel=?2 AND sequence>?3 ORDER BY sequence LIMIT ?4",
    ))?;
    let rows = db(db(
        s.query_map(params![session.as_str(), channel.as_str(), after, n], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        }),
    )?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter()
        .map(|(sequence, a, v, truncated, at, source)| {
            let tested_repository = source
                .map(|v| serde_json::from_str(&v).map_err(|_| corrupt("observation source")))
                .transpose()?;
            validate_source(tested_repository.as_ref())?;
            Ok(StoredBrowserObservation {
                session_id: session.clone(),
                sequence,
                channel,
                artifact_id: id_of(a)?,
                version_id: id_of(v)?,
                truncated: truncated != 0,
                recorded_at_ms: at,
                tested_repository,
            })
        })
        .collect()
}

macro_rules! browser_reads {
    ($t:ty) => {
        impl BrowserRead for $t {
            fn browser_session(
                &self,
                id: &Id,
            ) -> Result<Option<StoredBrowserSession>, StorageError> {
                Ok(sessions(self.conn, "id=?1", [id.as_str()])?
                    .into_iter()
                    .next())
            }
            fn browser_sessions(
                &self,
                filter: &BrowserFilter,
                after: Option<&Id>,
                n: u16,
            ) -> Result<Vec<StoredBrowserSession>, StorageError> {
                let n = limit(n)?;
                let after = after.map_or("", Id::as_str);
                match filter {
                    BrowserFilter::Scope(Some(p)) => sessions(
                        self.conn,
                        "project_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
                        params![p.0.as_str(), after, n],
                    ),
                    BrowserFilter::Scope(None) => sessions(
                        self.conn,
                        "project_id IS NULL AND id>?1 ORDER BY id LIMIT ?2",
                        params![after, n],
                    ),
                    BrowserFilter::Unsettled => sessions(
                        self.conn,
                        "state NOT IN ('Closed','Failed') AND id>?1 ORDER BY id LIMIT ?2",
                        params![after, n],
                    ),
                }
            }
            fn browser_observations(
                &self,
                session: &Id,
                channel: BrowserChannel,
                after: i64,
                n: u16,
            ) -> Result<Vec<StoredBrowserObservation>, StorageError> {
                observations(self.conn, session, channel, after, limit(n)?)
            }
        }
    };
}
browser_reads!(Snapshot<'_>);
browser_reads!(Unit<'_>);

fn validate_source(source: Option<&serde_json::Value>) -> Result<(), StorageError> {
    if let Some(source) = source {
        use devworks_core::protocol::support::Check;
        let record: devworks_core::protocol::records::TestedRepositoryRevision =
            serde_json::from_value(source.clone())
                .map_err(|_| invalid("tested repository revision"))?;
        record.check().map_err(invalid)?;
    }
    Ok(())
}
fn check(s: &StoredBrowserSession) -> Result<String, StorageError> {
    validate_source(s.tested_repository.as_ref())?;
    if s.tested_repository.is_some() != s.source_binding.is_some() {
        return Err(invalid("browser source requires its host binding"));
    }

    if !OWNER_KINDS.contains(&s.owner_kind.as_str()) {
        return Err(invalid(
            "browser session owner kind is not a principal kind",
        ));
    }
    if s.asset_id.is_empty() || s.asset_id.len() > 64 {
        return Err(invalid("browser asset id must be 1..=64 bytes"));
    }
    if s.detail.as_ref().is_some_and(|d| d.len() > 1024) {
        return Err(invalid("browser session detail must be at most 1024 bytes"));
    }
    if s.targets.len() > MAX_TARGETS
        || s.targets
            .iter()
            .any(|t| t.is_empty() || t.len() > 128 || !t.is_ascii())
    {
        return Err(invalid("browser session targets out of bounds"));
    }
    if s.observation_sequence < 0 {
        return Err(invalid("observation sequence must be non-negative"));
    }
    if s.phase == BrowserPhase::Ready && s.close_operation_id.is_some() {
        return Err(invalid(
            "a session whose close was requested is never ready",
        ));
    }
    serde_json::to_string(&s.targets).map_err(|e| invalid(e.to_string()))
}

impl BrowserRepository for Unit<'_> {
    fn insert_browser_session(&mut self, s: &StoredBrowserSession) -> Result<(), StorageError> {
        let targets = check(s)?;
        if s.revision != 1 || s.phase != BrowserPhase::Starting || s.close_operation_id.is_some() {
            return Err(invalid(
                "a new browser session starts at revision 1, Starting, with no close request",
            ));
        }
        if self.browser_session(&s.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "browser_session",
                id: s.id.as_str().to_owned(),
            });
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO browser_sessions(id,boot_id,state,owner_id,owner_kind,agent_session_id,project_id,root_id,assignment_id,asset_id,process_id,create_operation_id,uncertain_operation_id,targets,detail,observation_seq,created_at,updated_at,revision,tested_repository,source_binding) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?17,1,?18,?19)",
        ))?
        .execute(params![
            s.id.as_str(),
            s.boot_id.as_str(),
            s.phase.as_str(),
            s.owner.0.as_str(),
            s.owner_kind,
            s.agent_session_id.as_ref().map(Id::as_str),
            s.project_id.as_ref().map(|p| p.0.as_str()),
            s.root_id.as_ref().map(|r| r.0.as_str()),
            s.assignment_id.as_ref().map(|a| a.0.as_str()),
            s.asset_id,
            s.process_id.as_ref().map(Id::as_str),
            s.create_operation_id.0.as_str(),
            s.uncertain_operation_id.as_ref().map(|o| o.0.as_str()),
            targets,
            s.detail,
            s.observation_sequence,
            self.now,
            s.tested_repository.as_ref().map(serde_json::to_string).transpose().map_err(|_| invalid("browser source"))?,
            s.source_binding.as_ref().map(serde_json::to_string).transpose().map_err(|_| invalid("browser binding"))?,
        ]))?;
        Ok(())
    }

    fn update_browser_session(
        &mut self,
        s: &StoredBrowserSession,
        expected: i64,
    ) -> Result<(), StorageError> {
        let targets = check(s)?;
        let current = self
            .browser_session(&s.id)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "browser_session",
                id: s.id.as_str().to_owned(),
            })?;
        if current.revision != expected || s.revision != expected + 1 {
            return Err(lost(&s.id));
        }
        if current.boot_id != s.boot_id
            || current.owner != s.owner
            || current.owner_kind != s.owner_kind
            || current.agent_session_id != s.agent_session_id
            || current.project_id != s.project_id
            || current.root_id != s.root_id
            || current.assignment_id != s.assignment_id
            || current.asset_id != s.asset_id
            || current.tested_repository != s.tested_repository
            || current.source_binding != s.source_binding
            || current.create_operation_id != s.create_operation_id
            || current.phase.settled()
            || (current.process_id.is_some() && current.process_id != s.process_id)
            || s.observation_sequence < current.observation_sequence
            || (current.close_operation_id.is_some() && s.close_operation_id.is_none())
        {
            return Err(invalid(
                "a browser session keeps its identity, owner, scope, asset, process and close request; a settled one is final",
            ));
        }
        let n = db(db(self.conn.prepare_cached(
            "UPDATE browser_sessions SET state=?2,process_id=?3,uncertain_operation_id=?4,targets=?5,detail=?6,observation_seq=?7,updated_at=?8,revision=?9,close_operation_id=?11 WHERE id=?1 AND revision=?10",
        ))?
        .execute(params![
            s.id.as_str(),
            s.phase.as_str(),
            s.process_id.as_ref().map(Id::as_str),
            s.uncertain_operation_id.as_ref().map(|o| o.0.as_str()),
            targets,
            s.detail,
            s.observation_sequence,
            self.now,
            s.revision,
            expected,
            s.close_operation_id.as_ref().map(|o| o.0.as_str()),
        ]))?;
        if n != 1 {
            return Err(lost(&s.id));
        }
        Ok(())
    }

    fn set_last_action(
        &mut self,
        session: &Id,
        operation: &OperationId,
    ) -> Result<(), StorageError> {
        let n = db(self.conn.execute(
            "UPDATE browser_sessions SET last_action_operation_id=?2 WHERE id=?1",
            params![session.as_str(), operation.0.as_str()],
        ))?;
        if n != 1 {
            return Err(StorageError::NotFound {
                kind: "browser_session",
                id: session.as_str().to_owned(),
            });
        }
        Ok(())
    }

    fn next_observation_sequence(&mut self, session: &Id) -> Result<i64, StorageError> {
        let n = db(self.conn.execute(
            "UPDATE browser_sessions SET observation_seq=observation_seq+1 WHERE id=?1",
            [session.as_str()],
        ))?;
        if n != 1 {
            return Err(StorageError::NotFound {
                kind: "browser_session",
                id: session.as_str().to_owned(),
            });
        }
        db(self.conn.query_row(
            "SELECT observation_seq FROM browser_sessions WHERE id=?1",
            [session.as_str()],
            |r| r.get(0),
        ))
    }

    fn insert_browser_observation(
        &mut self,
        o: &StoredBrowserObservation,
    ) -> Result<(), StorageError> {
        let session =
            self.browser_session(&o.session_id)?
                .ok_or_else(|| StorageError::NotFound {
                    kind: "browser_session",
                    id: o.session_id.as_str().to_owned(),
                })?;
        validate_source(o.tested_repository.as_ref())?;
        if o.sequence <= 0 || o.sequence > session.observation_sequence {
            return Err(invalid(
                "an observation sequence must be allocated by its session",
            ));
        }
        let exists: i64 = db(self.conn.query_row(
            "SELECT count(*) FROM browser_observations WHERE session_id=?1 AND sequence=?2",
            params![o.session_id.as_str(), o.sequence],
            |r| r.get(0),
        ))?;
        if exists != 0 {
            return Err(StorageError::Duplicate {
                kind: "browser_observation",
                id: format!("{}:{}", o.session_id.as_str(), o.sequence),
            });
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO browser_observations(session_id,sequence,channel,artifact_id,version_id,truncated,recorded_at,tested_repository) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        ))?
        .execute(params![
            o.session_id.as_str(),
            o.sequence,
            o.channel.as_str(),
            o.artifact_id.as_str(),
            o.version_id.as_str(),
            i64::from(o.truncated),
            o.recorded_at_ms,
            o.tested_repository.as_ref().map(serde_json::to_string).transpose().map_err(|_| invalid("observation source"))?,
        ]))?;
        Ok(())
    }
}
