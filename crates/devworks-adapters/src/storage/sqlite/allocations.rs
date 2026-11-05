//! Durable allocation ownership and record-level fences. Eligibility stays in the work service.
use super::conn::db;
use super::repos::Unit;
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, ProjectId, Revision, RevisionDimension, RootId};
use rusqlite::{Connection, OptionalExtension, Row, params};

const COLUMNS: &str = "id,project_id,root_id,work_id,kind,queue_key,owner_id,session_id,ingress_namespace,invocation_id,state,parent_feature_id,execution_id,fence,expires_at,revision,work_revision,root_control_revision,queue_revision,created_at,feature_task_set_revision";
type Raw = (Vec<Option<String>>, [i64; 7], Option<i64>);
fn row(row: &Row<'_>) -> rusqlite::Result<Raw> {
    Ok((
        (0..13)
            .map(|i| row.get(i))
            .collect::<rusqlite::Result<_>>()?,
        [
            row.get(13)?,
            row.get(14)?,
            row.get(15)?,
            row.get(16)?,
            row.get(17)?,
            row.get(18)?,
            row.get(19)?,
        ],
        row.get(20)?,
    ))
}
fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn id(value: String) -> Result<Id, StorageError> {
    Id::new(value).map_err(|_| {
        StorageError::unavailable(Unavailable::Corrupt, "invalid stored allocation identifier")
    })
}
fn claim(
    (text, num, feature_task_set_revision): Raw,
) -> Result<StoredAllocationClaim, StorageError> {
    let mut text = text.into_iter();
    let mut next = || text.next().flatten();
    let need = |value: Option<String>| {
        value
            .ok_or_else(|| StorageError::unavailable(Unavailable::Corrupt, "NULL allocation field"))
    };
    Ok(StoredAllocationClaim {
        id: id(need(next())?)?,
        project_id: ProjectId(id(need(next())?)?),
        root_id: RootId(id(need(next())?)?),
        work_id: id(need(next())?)?,
        kind: need(next())?,
        queue_key: need(next())?,
        owner_id: ActorId(id(need(next())?)?),
        session_id: next().map(id).transpose()?,
        ingress_namespace: id(need(next())?)?,
        invocation_id: id(need(next())?)?,
        state: need(next())?,
        parent_feature_id: next().map(id).transpose()?,
        execution_id: next().map(id).transpose()?,
        fence: num[0],
        expires_at_ms: num[1],
        revision: num[2],
        work_revision: num[3],
        root_control_revision: num[4],
        queue_revision: num[5],
        created_at_ms: num[6],
        feature_task_set_revision,
    })
}
fn read<P: rusqlite::Params>(
    conn: &Connection,
    tail: &str,
    params: P,
) -> Result<Vec<StoredAllocationClaim>, StorageError> {
    let mut statement = db(conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM allocation_claims WHERE {tail}"
    )))?;
    db(db(statement.query_map(params, row))?.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(claim)
        .collect()
}
pub(super) fn one(
    conn: &Connection,
    value: &Id,
) -> Result<Option<StoredAllocationClaim>, StorageError> {
    Ok(read(conn, "id=?1", [value.as_str()])?.into_iter().next())
}
pub(super) fn page(
    conn: &Connection,
    project: &ProjectId,
    after: Option<&Id>,
    limit: u16,
) -> Result<Vec<StoredAllocationClaim>, StorageError> {
    if limit == 0 || limit > MAX_PAGE {
        return Err(invalid("allocation page limit out of bounds"));
    }
    read(
        conn,
        "project_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
        params![
            project.0.as_str(),
            after.map_or("", Id::as_str),
            i64::from(limit)
        ],
    )
}
pub(super) fn invocation(
    conn: &Connection,
    namespace: &Id,
    invocation: &Id,
) -> Result<Option<StoredAllocationClaim>, StorageError> {
    Ok(read(
        conn,
        "ingress_namespace=?1 AND invocation_id=?2",
        params![namespace.as_str(), invocation.as_str()],
    )?
    .into_iter()
    .next())
}
fn live(state: &str) -> bool {
    matches!(state, "Active" | "Transferred" | "Yielding")
}
fn check(c: &StoredAllocationClaim) -> Result<(), StorageError> {
    if c.fence < 1
        || c.revision < 1
        || c.work_revision < 1
        || c.root_control_revision < 1
        || c.queue_revision < 1
        || c.feature_task_set_revision.is_some_and(|r| r < 1)
        || c.created_at_ms < 0
        || c.expires_at_ms < c.created_at_ms
    {
        return Err(invalid("allocation counters and lease are invalid"));
    }
    if !matches!(
        c.state.as_str(),
        "Active" | "Transferred" | "Yielding" | "Released" | "Expired" | "Yielded" | "Invalidated"
    ) || !matches!(c.kind.as_str(), "feature" | "task" | "issue")
        || c.queue_key.is_empty()
        || c.queue_key.len() > 160
    {
        return Err(invalid("invalid allocation state, kind or queue"));
    }
    Ok(())
}
fn conflict(c: &StoredAllocationClaim, expected: i64) -> Result<StorageError, StorageError> {
    Ok(StorageError::conflict(
        RevisionDimension::Claim,
        "allocation_claim",
        c.id.clone(),
        Revision::new(expected).map_err(|_| invalid("expected claim revision is positive"))?,
        Revision::new(c.revision).map_err(|_| invalid("claim revision is positive"))?,
    ))
}
fn source(unit: &Unit<'_>, c: &StoredAllocationClaim, original: bool) -> Result<(), StorageError> {
    let work = unit
        .work(&c.work_id)?
        .ok_or_else(|| StorageError::NotFound {
            kind: "work",
            id: c.work_id.as_str().into(),
        })?;
    if work.project_id != c.project_id
        || work.parent_feature_id != c.parent_feature_id
        || work.kind != c.kind
        || work.revision != c.work_revision
    {
        return Err(invalid("allocation source work or association changed"));
    }
    if original {
        let expected_root = work.parent_feature_id.as_ref().unwrap_or(&work.id);
        if &c.root_id.0 != expected_root || c.queue_key != work.queue() {
            return Err(invalid("allocation root or queue does not match work"));
        }
        if unit.queue_revision(&c.project_id, &c.queue_key)? != c.queue_revision {
            return Err(invalid("allocation queue changed"));
        }
        let expected_tasks = if work.is_feature() {
            Some(unit.queue_revision(&c.project_id, &feature_queue(&work.id))?)
        } else {
            None
        };
        if c.feature_task_set_revision != expected_tasks {
            return Err(invalid("allocation feature task set changed"));
        }
        let control = unit
            .root_control(&c.root_id)?
            .unwrap_or_else(|| StoredRootControl::initial(c.root_id.clone()));
        if control.revision != c.root_control_revision
            || control.desired != DesiredExecution::Enabled
        {
            return Err(invalid("allocation root control changed or is paused"));
        }
    }
    if let Some(session) = &c.session_id {
        let session = unit
            .session(session)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "session",
                id: session.as_str().into(),
            })?;
        if session
            .project_id
            .as_ref()
            .is_some_and(|p| p != &c.project_id)
        {
            return Err(StorageError::ScopeDenied);
        }
    }
    Ok(())
}
fn executing(conn: &Connection, work: &Id, execution: Option<&Id>) -> Result<bool, StorageError> {
    db(conn.query_row("SELECT EXISTS(SELECT 1 FROM executions WHERE phase<>'Stopped' AND (root_id=?1 OR id=?2)) OR EXISTS(SELECT 1 FROM assignments a JOIN executions e ON e.id=a.execution_id WHERE a.work_id=?1 AND a.state IN ('Ready','Active','Waiting') AND e.phase<>'Stopped')",params![work.as_str(),execution.map(Id::as_str)],|row|row.get(0)))
}
fn duplicate(
    conn: &Connection,
    c: &StoredAllocationClaim,
    exclude: Option<&Id>,
) -> Result<(), StorageError> {
    let collision:Option<String>=db(conn.query_row("SELECT id FROM allocation_claims WHERE id<>?1 AND state IN ('Active','Transferred','Yielding') AND (work_id=?2 OR (project_id=?3 AND queue_key='project' AND ?4='project')) LIMIT 1",params![exclude.map_or("",Id::as_str),c.work_id.as_str(),c.project_id.0.as_str(),c.queue_key],|row|row.get(0)).optional())?;
    if let Some(value) = collision {
        return Err(StorageError::Duplicate {
            kind: "allocation_claim",
            id: value,
        });
    }
    Ok(())
}
pub(super) fn insert(unit: &mut Unit<'_>, c: &StoredAllocationClaim) -> Result<(), StorageError> {
    check(c)?;
    if c.state != "Active"
        || c.revision != 1
        || c.execution_id.is_some()
        || c.expires_at_ms <= unit.now_ms()
    {
        return Err(invalid(
            "a new allocation is active with a future lease and no execution",
        ));
    }
    if one(unit.conn, &c.id)?.is_some()
        || invocation(unit.conn, &c.ingress_namespace, &c.invocation_id)?.is_some()
    {
        return Err(StorageError::Duplicate {
            kind: "allocation_claim",
            id: c.id.as_str().into(),
        });
    }
    source(unit, c, true)?;
    if executing(unit.conn, &c.work_id, None)? {
        return Err(StorageError::Duplicate {
            kind: "allocation_execution",
            id: c.work_id.as_str().into(),
        });
    }
    duplicate(unit.conn, c, None)?;
    let previous: i64 = db(unit.conn.query_row(
        "SELECT COALESCE(MAX(fence),0) FROM allocation_claims WHERE work_id=?1",
        [c.work_id.as_str()],
        |row| row.get(0),
    ))?;
    if c.fence <= previous {
        return Err(invalid("allocation fence must advance prior ownership"));
    }
    db(unit.conn.execute("INSERT INTO allocation_claims(id,project_id,root_id,work_id,kind,queue_key,owner_id,session_id,ingress_namespace,invocation_id,fence,expires_at,state,revision,work_revision,parent_feature_id,root_control_revision,queue_revision,feature_task_set_revision,created_at,execution_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",params![c.id.as_str(),c.project_id.0.as_str(),c.root_id.0.as_str(),c.work_id.as_str(),c.kind,c.queue_key,c.owner_id.0.as_str(),c.session_id.as_ref().map(Id::as_str),c.ingress_namespace.as_str(),c.invocation_id.as_str(),c.fence,c.expires_at_ms,c.state,c.revision,c.work_revision,c.parent_feature_id.as_ref().map(Id::as_str),c.root_control_revision,c.queue_revision,c.feature_task_set_revision,c.created_at_ms,c.execution_id.as_ref().map(Id::as_str)]))?;
    Ok(())
}
pub(super) fn update(
    unit: &mut Unit<'_>,
    c: &StoredAllocationClaim,
    expected: i64,
) -> Result<(), StorageError> {
    check(c)?;
    let old = one(unit.conn, &c.id)?.ok_or_else(|| StorageError::NotFound {
        kind: "allocation_claim",
        id: c.id.as_str().into(),
    })?;
    if old.revision != expected {
        return Err(conflict(&old, expected)?);
    }
    if c.revision
        != expected
            .checked_add(1)
            .ok_or_else(|| invalid("claim revision overflow"))?
        || c.work_id != old.work_id
        || c.project_id != old.project_id
        || c.kind != old.kind
        || c.root_id != old.root_id
        || c.queue_key != old.queue_key
        || c.ingress_namespace != old.ingress_namespace
        || c.invocation_id != old.invocation_id
        || c.created_at_ms != old.created_at_ms
        || c.fence < old.fence
    {
        return Err(invalid(
            "allocation identity is immutable and revision advances once",
        ));
    }
    if (c.owner_id != old.owner_id || c.session_id != old.session_id) && c.fence <= old.fence {
        return Err(invalid("ownership transfer advances the fence"));
    }
    if c.fence > old.fence {
        let previous: i64 = db(unit.conn.query_row(
            "SELECT COALESCE(MAX(fence),0) FROM allocation_claims WHERE work_id=?1 AND id<>?2",
            params![c.work_id.as_str(), c.id.as_str()],
            |row| row.get(0),
        ))?;
        if c.fence <= previous {
            return Err(invalid("allocation fence must advance prior ownership"));
        }
    }
    let allowed = match old.state.as_str() {
        "Active" => matches!(
            c.state.as_str(),
            "Active" | "Transferred" | "Released" | "Expired" | "Invalidated"
        ),
        "Transferred" => matches!(
            c.state.as_str(),
            "Transferred" | "Yielding" | "Released" | "Expired"
        ),
        "Yielding" => matches!(
            c.state.as_str(),
            "Yielding" | "Transferred" | "Yielded" | "Released"
        ),
        "Yielded" => c.state == "Active",
        _ => false,
    };
    if !allowed {
        return Err(invalid("invalid allocation transition"));
    }
    if old.execution_id.is_some()
        && c.execution_id != old.execution_id
        && executing(unit.conn, &old.work_id, old.execution_id.as_ref())?
    {
        return Err(invalid(
            "live allocation execution binding cannot be replaced",
        ));
    }
    if c.state == "Expired" && c.expires_at_ms > unit.now_ms() {
        return Err(invalid("allocation lease has not expired"));
    }
    if !live(&c.state) && executing(unit.conn, &old.work_id, old.execution_id.as_ref())? {
        return Err(invalid(
            "live execution retains allocation ownership until quiescent",
        ));
    }
    if c.state == "Active" {
        if c.expires_at_ms <= unit.now_ms() || c.execution_id.is_some() {
            return Err(invalid(
                "active allocation needs a future lease and no execution",
            ));
        }
        source(unit, c, old.state == "Yielded")?;
        if old.state == "Yielded" && c.fence <= old.fence {
            return Err(invalid("reallocation advances the ownership fence"));
        }
    } else if live(&c.state) {
        source(unit, c, false)?;
        if let Some(execution) = &c.execution_id {
            let root: Option<String> = db(unit
                .conn
                .query_row(
                    "SELECT root_id FROM executions WHERE id=?1",
                    [execution.as_str()],
                    |row| row.get(0),
                )
                .optional())?;
            if root.as_deref() != Some(c.root_id.0.as_str()) {
                return Err(invalid("allocation execution belongs to its original root"));
            }
        } else {
            return Err(invalid("transferred allocation names its execution"));
        }
    }
    if live(&c.state) {
        duplicate(unit.conn, c, Some(&c.id))?;
    }
    db(unit.conn.execute("UPDATE allocation_claims SET owner_id=?2,session_id=?3,fence=?4,expires_at=?5,state=?6,revision=?7,work_revision=?8,parent_feature_id=?9,root_control_revision=?10,queue_revision=?11,feature_task_set_revision=?12,execution_id=?13 WHERE id=?1 AND revision=?14",params![c.id.as_str(),c.owner_id.0.as_str(),c.session_id.as_ref().map(Id::as_str),c.fence,c.expires_at_ms,c.state,c.revision,c.work_revision,c.parent_feature_id.as_ref().map(Id::as_str),c.root_control_revision,c.queue_revision,c.feature_task_set_revision,c.execution_id.as_ref().map(Id::as_str),expected]))?;
    Ok(())
}
