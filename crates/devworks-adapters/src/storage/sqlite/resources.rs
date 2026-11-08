//! SQLite resource reservation and owned-process records: `resources` (migration 1, extended by
//! migration 10), `resource_leases` and `owned_processes` (migration 10).
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::RevisionDimension;
use devworks_core::protocol::{
    ActorId, AssignmentId, Id, OperationId, ProjectId, Revision, RootId,
};
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
fn limit(l: u16) -> Result<i64, StorageError> {
    if l == 0 || l > MAX_PAGE {
        return Err(invalid(format!("limit must be 1..={MAX_PAGE}")));
    }
    Ok(i64::from(l))
}
fn lost(kind: &'static str, id: &Id) -> StorageError {
    StorageError::LeaseLost {
        kind,
        id: id.as_str().to_owned(),
    }
}

// ---------------------------------------------------------------------------------------------
// Resources

const RESOURCE_COLUMNS: &str = "id,name,canonical_identity,scope,project_id,physical,capacity,labels_json,revision,fence_seq,queue_seq,created_at,updated_at";
type ResourceRaw = (
    String,
    String,
    String,
    String,
    Option<String>,
    i64,
    i64,
    String,
    i64,
    i64,
    i64,
    i64,
    i64,
);
fn resource_row(r: &Row<'_>) -> rusqlite::Result<ResourceRaw> {
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
    ))
}
fn resource_of(raw: ResourceRaw) -> Result<StoredResource, StorageError> {
    let (id, name, key, scope, project, physical, capacity, labels, revision, fence, queue, c, u) =
        raw;
    let scope = match scope.as_str() {
        "machine" => ResourceScopeKind::Machine,
        "project" => ResourceScopeKind::Project,
        other => return Err(corrupt(format!("stored resource scope {other}"))),
    };
    let labels: Vec<String> = serde_json::from_str(&labels)
        .map_err(|e| corrupt(format!("stored resource labels: {e}")))?;
    Ok(StoredResource {
        id: id_of(id)?,
        name,
        canonical_key: key,
        scope,
        project_id: opt_id(project)?.map(ProjectId),
        physical: physical == 1,
        capacity,
        labels,
        revision,
        fence_seq: fence,
        queue_seq: queue,
        created_at_ms: c,
        updated_at_ms: u,
    })
}
fn one_resource<P: rusqlite::Params>(
    conn: &Connection,
    filter: &str,
    p: P,
) -> Result<Option<StoredResource>, StorageError> {
    let raw = db(db(conn.prepare_cached(&format!(
        "SELECT {RESOURCE_COLUMNS} FROM resources WHERE {filter}"
    )))?
    .query_row(p, resource_row)
    .optional())?;
    raw.map(resource_of).transpose()
}
fn resources(
    conn: &Connection,
    filter: &ResourceFilter,
    after: Option<&str>,
    n: u16,
) -> Result<Vec<StoredResource>, StorageError> {
    let n = limit(n)?;
    let after = after.unwrap_or("");
    let rows = match filter {
        ResourceFilter::All => {
            let mut s = db(conn.prepare_cached(&format!(
                "SELECT {RESOURCE_COLUMNS} FROM resources WHERE name<>'' AND canonical_identity>?1 ORDER BY canonical_identity LIMIT ?2"
            )))?;
            db(db(s.query_map(params![after, n], resource_row))?
                .collect::<rusqlite::Result<Vec<_>>>())?
        }
        ResourceFilter::Project(p) => {
            let mut s = db(conn.prepare_cached(&format!(
                "SELECT {RESOURCE_COLUMNS} FROM resources WHERE name<>'' AND (scope='machine' OR project_id=?1) AND canonical_identity>?2 ORDER BY canonical_identity LIMIT ?3"
            )))?;
            db(
                db(s.query_map(params![p.0.as_str(), after, n], resource_row))?
                    .collect::<rusqlite::Result<Vec<_>>>(),
            )?
        }
    };
    rows.into_iter().map(resource_of).collect()
}

// ---------------------------------------------------------------------------------------------
// Leases

const LEASE_COLUMNS: &str = "id,resource_id,state,end_reason,owner_id,holder_key,session_id,project_id,root_id,assignment_id,boot_id,purpose,priority,units,lease_ms,queue_seq,fence,requested_at,granted_at,renewed_at,expires_at,ended_at,unreachable_since,revision";
struct LeaseRaw {
    text: [Option<String>; 12],
    num: [i64; 7],
    opt: [Option<i64>; 4],
    revision: i64,
}
fn lease_row(r: &Row<'_>) -> rusqlite::Result<LeaseRaw> {
    Ok(LeaseRaw {
        text: [
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
        ],
        num: [
            r.get(12)?,
            r.get(13)?,
            r.get(14)?,
            r.get(15)?,
            r.get(16)?,
            r.get(17)?,
            r.get(20)?,
        ],
        opt: [r.get(18)?, r.get(19)?, r.get(21)?, r.get(22)?],
        revision: r.get(23)?,
    })
}
fn lease_of(raw: LeaseRaw) -> Result<StoredLease, StorageError> {
    let [
        id,
        resource,
        state,
        end,
        owner,
        holder,
        session,
        project,
        root,
        assignment,
        boot,
        purpose,
    ] = raw.text;
    let need = |v: Option<String>| v.ok_or_else(|| corrupt("lease column is NULL"));
    let state = need(state)?;
    let phase =
        LeasePhase::parse(&state).ok_or_else(|| corrupt(format!("stored lease state {state}")))?;
    let end_reason = end
        .map(|e| LeaseEndReason::parse(&e).ok_or_else(|| corrupt(format!("stored lease end {e}"))))
        .transpose()?;
    let [
        priority,
        units,
        lease_ms,
        queue_seq,
        fence,
        requested_at_ms,
        expires_at_ms,
    ] = raw.num;
    let [
        granted_at_ms,
        renewed_at_ms,
        ended_at_ms,
        unreachable_since_ms,
    ] = raw.opt;
    Ok(StoredLease {
        id: id_of(need(id)?)?,
        resource_id: id_of(need(resource)?)?,
        phase,
        end_reason,
        owner: ActorId(id_of(need(owner)?)?),
        holder_key: need(holder)?,
        session_id: opt_id(session)?,
        project_id: opt_id(project)?.map(ProjectId),
        root_id: opt_id(root)?.map(RootId),
        assignment_id: opt_id(assignment)?.map(AssignmentId),
        boot_id: id_of(need(boot)?)?,
        purpose,
        priority,
        units,
        lease_ms,
        queue_seq,
        fence,
        requested_at_ms,
        granted_at_ms,
        renewed_at_ms,
        expires_at_ms,
        ended_at_ms,
        unreachable_since_ms,
        revision: raw.revision,
    })
}
fn leases<P: rusqlite::Params>(
    conn: &Connection,
    tail: &str,
    p: P,
) -> Result<Vec<StoredLease>, StorageError> {
    let mut s = db(conn.prepare_cached(&format!(
        "SELECT {LEASE_COLUMNS} FROM resource_leases WHERE {tail}"
    )))?;
    db(db(s.query_map(p, lease_row))?.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(lease_of)
        .collect()
}
fn one_lease<P: rusqlite::Params>(
    conn: &Connection,
    tail: &str,
    p: P,
) -> Result<Option<StoredLease>, StorageError> {
    Ok(leases(conn, tail, p)?.into_iter().next())
}
fn open_leases(
    conn: &Connection,
    filter: &OpenLeaseFilter,
    after: Option<&Id>,
    n: u16,
) -> Result<Vec<StoredLease>, StorageError> {
    let n = limit(n)?;
    let after = after.map_or("", Id::as_str);
    match filter {
        OpenLeaseFilter::All => leases(
            conn,
            "state<>'Released' AND id>?1 ORDER BY id LIMIT ?2",
            params![after, n],
        ),
        OpenLeaseFilter::PriorBoots(boot) => leases(
            conn,
            "state<>'Released' AND boot_id<>?1 AND id>?2 ORDER BY id LIMIT ?3",
            params![boot.as_str(), after, n],
        ),
        OpenLeaseFilter::Session(session) => leases(
            conn,
            "state<>'Released' AND session_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
            params![session.as_str(), after, n],
        ),
        OpenLeaseFilter::Root(root) => leases(
            conn,
            "state<>'Released' AND root_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
            params![root.0.as_str(), after, n],
        ),
        OpenLeaseFilter::Project(project) => leases(
            conn,
            "state<>'Released' AND project_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
            params![project.0.as_str(), after, n],
        ),
    }
}
fn check_lease(l: &StoredLease) -> Result<(), StorageError> {
    if !(0..=3).contains(&l.priority) || !(1..=500).contains(&l.units) || l.lease_ms <= 0 {
        return Err(invalid(
            "lease priority, units or time to live out of bounds",
        ));
    }
    if l.purpose
        .as_ref()
        .is_some_and(|p| p.is_empty() || p.chars().count() > 500)
    {
        return Err(invalid("lease purpose must be 1..=500 characters"));
    }
    if l.holder_key.is_empty() || l.holder_key.len() > 512 {
        return Err(invalid("lease holder key must be 1..=512 bytes"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Owned processes

const PROCESS_COLUMNS: &str = "id,boot_id,state,pid,birth,process_group,platform,program,argv_sha256,operation_id,lease_id,actor_id,session_id,project_id,root_id,assignment_id,exit_code,detail,created_at,updated_at,revision";
type ProcessRaw = (Vec<Option<String>>, [Option<i64>; 4], [i64; 3]);
fn process_row(r: &Row<'_>) -> rusqlite::Result<ProcessRaw> {
    let text = [0, 1, 2, 4, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 17]
        .iter()
        .map(|i| r.get(*i))
        .collect::<rusqlite::Result<Vec<Option<String>>>>()?;
    Ok((
        text,
        [r.get(3)?, r.get(5)?, r.get(16)?, None],
        [r.get(18)?, r.get(19)?, r.get(20)?],
    ))
}
fn process_of((text, opt, num): ProcessRaw) -> Result<StoredProcess, StorageError> {
    let mut t = text.into_iter();
    let mut next = || t.next().flatten();
    let need = |v: Option<String>| v.ok_or_else(|| corrupt("process column is NULL"));
    let id = id_of(need(next())?)?;
    let boot_id = id_of(need(next())?)?;
    let state = need(next())?;
    let phase = ProcessPhase::parse(&state)
        .ok_or_else(|| corrupt(format!("stored process state {state}")))?;
    let birth = next();
    let platform = need(next())?;
    let program = need(next())?;
    let argv_sha256 = need(next())?;
    let operation_id = opt_id(next())?.map(OperationId);
    let lease_id = opt_id(next())?;
    let actor = ActorId(id_of(need(next())?)?);
    let session_id = opt_id(next())?;
    let project_id = opt_id(next())?.map(ProjectId);
    let root_id = opt_id(next())?.map(RootId);
    let assignment_id = opt_id(next())?.map(AssignmentId);
    let detail = next();
    Ok(StoredProcess {
        id,
        boot_id,
        phase,
        pid: opt[0],
        birth,
        group: opt[1],
        platform,
        program,
        argv_sha256,
        operation_id,
        lease_id,
        actor,
        session_id,
        project_id,
        root_id,
        assignment_id,
        exit_code: opt[2],
        detail,
        created_at_ms: num[0],
        updated_at_ms: num[1],
        revision: num[2],
    })
}
fn processes<P: rusqlite::Params>(
    conn: &Connection,
    tail: &str,
    p: P,
) -> Result<Vec<StoredProcess>, StorageError> {
    let mut s = db(conn.prepare_cached(&format!(
        "SELECT {PROCESS_COLUMNS} FROM owned_processes WHERE {tail}"
    )))?;
    db(db(s.query_map(p, process_row))?.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(process_of)
        .collect()
}
const UNSETTLED: &str = "state NOT IN ('Reaped','Failed')";

const CONTAINER_COLUMNS: &str = "id,engine,state,image,actor_id,owner_kind,session_id,project_id,root_id,assignment_id,create_operation_id,last_operation_id,detail,revision,created_at,updated_at";
#[allow(clippy::type_complexity)] // Fixed SQLite column tuple, decoded once into StoredContainer.
type ContainerColumns = (
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
    i64,
    i64,
    i64,
);
fn container_row(r: &Row<'_>) -> rusqlite::Result<ContainerColumns> {
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
        r.get(15)?,
    ))
}
fn container_of(r: ContainerColumns) -> Result<StoredContainer, StorageError> {
    let (
        id,
        engine,
        phase,
        image,
        actor,
        owner_kind,
        session,
        project,
        root,
        assignment,
        create,
        last,
        detail,
        revision,
        created,
        updated,
    ) = r;
    Ok(StoredContainer {
        id: id_of(id)?,
        engine,
        phase: ContainerPhase::parse(&phase).ok_or_else(|| corrupt("unknown container phase"))?,
        image,
        actor: ActorId(id_of(actor)?),
        owner_kind,
        session_id: opt_id(session)?,
        project_id: ProjectId(id_of(project)?),
        root_id: opt_id(root)?.map(RootId),
        assignment_id: opt_id(assignment)?.map(AssignmentId),
        create_operation_id: OperationId(id_of(create)?),
        last_operation_id: OperationId(id_of(last)?),
        detail,
        revision,
        created_at_ms: created,
        updated_at_ms: updated,
    })
}
fn containers<P: rusqlite::Params>(
    conn: &Connection,
    tail: &str,
    p: P,
) -> Result<Vec<StoredContainer>, StorageError> {
    let mut s = db(conn.prepare_cached(&format!(
        "SELECT {CONTAINER_COLUMNS} FROM owned_containers WHERE {tail}"
    )))?;
    db(db(s.query_map(p, container_row))?.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(container_of)
        .collect()
}

macro_rules! resource_reads {
    ($t:ty) => {
        impl ResourceRead for $t {
            fn resource(&self, id: &Id) -> Result<Option<StoredResource>, StorageError> {
                one_resource(self.conn, "id=?1 AND name<>''", [id.as_str()])
            }
            fn resource_by_key(
                &self,
                key: &str,
            ) -> Result<Option<StoredResource>, StorageError> {
                one_resource(self.conn, "canonical_identity=?1 AND name<>''", [key])
            }
            fn resources(
                &self,
                filter: &ResourceFilter,
                after: Option<&str>,
                limit: u16,
            ) -> Result<Vec<StoredResource>, StorageError> {
                resources(self.conn, filter, after, limit)
            }
            fn lease(&self, id: &Id) -> Result<Option<StoredLease>, StorageError> {
                one_lease(self.conn, "id=?1", [id.as_str()])
            }
            fn holder_lease(
                &self,
                resource: &Id,
                holder_key: &str,
            ) -> Result<Option<StoredLease>, StorageError> {
                one_lease(
                    self.conn,
                    "resource_id=?1 AND holder_key=?2 AND state<>'Released'",
                    params![resource.as_str(), holder_key],
                )
            }
            fn holders(
                &self,
                resource: &Id,
                after: i64,
                n: u16,
            ) -> Result<Vec<StoredLease>, StorageError> {
                let n = limit(n)?;
                leases(
                    self.conn,
                    "resource_id=?1 AND state IN ('Held','Releasing','Uncertain') AND fence>?2 ORDER BY fence LIMIT ?3",
                    params![resource.as_str(), after, n],
                )
            }
            fn waiters(
                &self,
                resource: &Id,
                after: Option<QueuePosition>,
                n: u16,
            ) -> Result<Vec<StoredLease>, StorageError> {
                let n = limit(n)?;
                let (p, q) = after.map_or((i64::MAX, 0), |a| (a.priority, a.queue_seq));
                leases(
                    self.conn,
                    "resource_id=?1 AND state='Waiting' AND (priority<?2 OR (priority=?2 AND queue_seq>?3)) ORDER BY priority DESC,queue_seq LIMIT ?4",
                    params![resource.as_str(), p, q, n],
                )
            }
            fn open_leases(
                &self,
                filter: &OpenLeaseFilter,
                after: Option<&Id>,
                limit: u16,
            ) -> Result<Vec<StoredLease>, StorageError> {
                open_leases(self.conn, filter, after, limit)
            }
            fn process(&self, id: &Id) -> Result<Option<StoredProcess>, StorageError> {
                Ok(processes(self.conn, "id=?1", [id.as_str()])?.into_iter().next())
            }
            fn processes(
                &self,
                filter: &ProcessFilter,
                after: Option<&Id>,
                n: u16,
            ) -> Result<Vec<StoredProcess>, StorageError> {
                let n = limit(n)?;
                let after = after.map_or("", Id::as_str);
                match filter {
                    ProcessFilter::Operation(operation) => processes(
                        self.conn,
                        "operation_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
                        params![operation.0.as_str(), after, n],
                    ),
                    ProcessFilter::Lease(lease) => processes(
                        self.conn,
                        "lease_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
                        params![lease.as_str(), after, n],
                    ),
                    ProcessFilter::UnsettledPriorBoots(boot) => processes(
                        self.conn,
                        &format!("{UNSETTLED} AND boot_id<>?1 AND id>?2 ORDER BY id LIMIT ?3"),
                        params![boot.as_str(), after, n],
                    ),
                    ProcessFilter::Unsettled => processes(
                        self.conn,
                        &format!("{UNSETTLED} AND id>?1 ORDER BY id LIMIT ?2"),
                        params![after, n],
                    ),
                }
            }
            fn container(&self, id: &Id) -> Result<Option<StoredContainer>, StorageError> {
                Ok(containers(self.conn, "id=?1", [id.as_str()])?.into_iter().next())
            }
            fn containers(&self, filter: &ContainerFilter, after: Option<&Id>, n: u16) -> Result<Vec<StoredContainer>, StorageError> {
                let n = limit(n)?;
                let after = after.map_or("", Id::as_str);
                match filter {
                    ContainerFilter::AgentWorkMayRun => containers(self.conn,
                        "owner_kind='Agent' AND state IN ('Creating','Starting','Running','Stopping','Uncertain') AND id>?1 ORDER BY id LIMIT ?2",
                        params![after, n]),
                    ContainerFilter::Project(project) => containers(self.conn,
                        "project_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
                        params![project.0.as_str(), after, n]),
                }
            }
        }
    };
}
resource_reads!(Snapshot<'_>);
resource_reads!(Unit<'_>);

impl ResourceRepository for Unit<'_> {
    fn insert_resource(&mut self, r: NewResource) -> Result<StoredResource, StorageError> {
        let key = r.canonical_key();
        let name_ok = !r.name.is_empty()
            && r.name.len() <= 128
            && r.name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-');
        if !name_ok || r.name != r.name.to_ascii_lowercase() {
            return Err(invalid(
                "resource name must be a case-folded opaque identifier",
            ));
        }
        if (r.scope == ResourceScopeKind::Project) != r.project_id.is_some() {
            return Err(invalid(
                "a project resource names its project; a machine one none",
            ));
        }
        if !(1..=500).contains(&r.capacity)
            || r.labels.len() > 20
            || r.labels
                .iter()
                .any(|l| l.is_empty() || l.chars().count() > 64)
        {
            return Err(invalid("resource capacity or labels out of bounds"));
        }
        if self.resource_by_key(&key)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "resource",
                id: key,
            });
        }
        if let Some(p) = &r.project_id
            && !db(db(self
                .conn
                .prepare_cached("SELECT 1 FROM projects WHERE id=?1 AND lifecycle<>'Deleted'"))?
            .exists([p.0.as_str()]))?
        {
            return Err(StorageError::NotFound {
                kind: "project",
                id: p.0.as_str().to_owned(),
            });
        }
        let labels = serde_json::to_string(&r.labels).map_err(|e| invalid(e.to_string()))?;
        db(db(self.conn.prepare_cached(
            "INSERT INTO resources(id,canonical_identity,capacity,revision,name,scope,project_id,physical,labels_json,fence_seq,queue_seq,created_at,updated_at) VALUES(?1,?2,?3,1,?4,?5,?6,?7,?8,0,0,?9,?9)",
        ))?
        .execute(params![
            r.id.as_str(),
            key,
            r.capacity,
            r.name,
            match r.scope {
                ResourceScopeKind::Machine => "machine",
                ResourceScopeKind::Project => "project",
            },
            r.project_id.as_ref().map(|p| p.0.as_str()),
            i64::from(r.physical),
            labels,
            self.now,
        ]))?;
        self.resource(&r.id)?
            .ok_or_else(|| corrupt("resource vanished inside its transaction"))
    }
    fn redefine_resource(
        &mut self,
        id: &Id,
        expected: i64,
        capacity: i64,
        labels: &[String],
        physical: bool,
    ) -> Result<StoredResource, StorageError> {
        let current = self.resource(id)?.ok_or_else(|| StorageError::NotFound {
            kind: "resource",
            id: id.as_str().to_owned(),
        })?;
        if current.revision != expected {
            return Err(StorageError::conflict(
                RevisionDimension::Claim,
                "resource",
                id.clone(),
                Revision::new(expected.max(1)).map_err(|_| invalid("revision"))?,
                Revision::new(current.revision).map_err(|_| corrupt("stored revision"))?,
            ));
        }
        if !(1..=500).contains(&capacity) || labels.len() > 20 {
            return Err(invalid("resource capacity or labels out of bounds"));
        }
        let labels = serde_json::to_string(labels).map_err(|e| invalid(e.to_string()))?;
        db(db(self.conn.prepare_cached(
            "UPDATE resources SET capacity=?1,labels_json=?2,physical=?3,revision=revision+1,updated_at=?4 WHERE id=?5",
        ))?
        .execute(params![capacity, labels, i64::from(physical), self.now, id.as_str()]))?;
        self.resource(id)?
            .ok_or_else(|| corrupt("resource vanished inside its transaction"))
    }
    fn next_fence(&mut self, resource: &Id) -> Result<i64, StorageError> {
        db(db(self.conn.prepare_cached(
            "UPDATE resources SET fence_seq=fence_seq+1 WHERE id=?1 AND name<>'' RETURNING fence_seq",
        ))?
        .query_row([resource.as_str()], |r| r.get(0))
        .optional())?
        .ok_or_else(|| StorageError::NotFound {
            kind: "resource",
            id: resource.as_str().to_owned(),
        })
    }
    fn next_queue_ticket(&mut self, resource: &Id) -> Result<i64, StorageError> {
        db(db(self.conn.prepare_cached(
            "UPDATE resources SET queue_seq=queue_seq+1 WHERE id=?1 AND name<>'' RETURNING queue_seq",
        ))?
        .query_row([resource.as_str()], |r| r.get(0))
        .optional())?
        .ok_or_else(|| StorageError::NotFound {
            kind: "resource",
            id: resource.as_str().to_owned(),
        })
    }
    fn insert_lease(&mut self, l: &StoredLease) -> Result<(), StorageError> {
        check_lease(l)?;
        if l.revision != 1 {
            return Err(invalid("a new lease starts at revision 1"));
        }
        if self.holder_lease(&l.resource_id, &l.holder_key)?.is_some()
            && l.phase != LeasePhase::Released
        {
            return Err(StorageError::Duplicate {
                kind: "resource_lease",
                id: l.holder_key.clone(),
            });
        }
        if self.lease(&l.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "resource_lease",
                id: l.id.as_str().to_owned(),
            });
        }
        write_lease(
            self.conn,
            l,
            "INSERT INTO resource_leases(id,resource_id,state,end_reason,owner_id,holder_key,session_id,project_id,root_id,assignment_id,boot_id,purpose,priority,units,lease_ms,queue_seq,fence,requested_at,granted_at,renewed_at,expires_at,ended_at,unreachable_since,revision) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)",
            None,
        )
    }
    fn update_lease(&mut self, l: &StoredLease, expected: i64) -> Result<(), StorageError> {
        check_lease(l)?;
        let current = self.lease(&l.id)?.ok_or_else(|| StorageError::NotFound {
            kind: "resource_lease",
            id: l.id.as_str().to_owned(),
        })?;
        if current.revision != expected || l.revision != expected + 1 {
            return Err(lost("resource_lease", &l.id));
        }
        if current.resource_id != l.resource_id
            || current.holder_key != l.holder_key
            || current.owner != l.owner
            || current.phase == LeasePhase::Released
            || (current.fence != 0 && current.fence != l.fence)
        {
            return Err(invalid(
                "a lease keeps its resource, holder and fence; an ended lease is final",
            ));
        }
        write_lease(
            self.conn,
            l,
            "UPDATE resource_leases SET resource_id=?2,state=?3,end_reason=?4,owner_id=?5,holder_key=?6,session_id=?7,project_id=?8,root_id=?9,assignment_id=?10,boot_id=?11,purpose=?12,priority=?13,units=?14,lease_ms=?15,queue_seq=?16,fence=?17,requested_at=?18,granted_at=?19,renewed_at=?20,expires_at=?21,ended_at=?22,unreachable_since=?23,revision=?24 WHERE id=?1 AND revision=?25",
            Some(expected),
        )
    }
    fn insert_process(&mut self, p: NewProcess) -> Result<StoredProcess, StorageError> {
        let boot = self
            .info
            .boot_id
            .clone()
            .ok_or_else(|| StorageError::IncompatibleSchema("store is read-only".into()))?;
        if p.program.is_empty() || p.program.chars().count() > 256 || p.argv_sha256.len() != 64 {
            return Err(invalid("process program or argv digest out of bounds"));
        }
        if !matches!(p.platform.as_str(), "unix" | "windows") {
            return Err(invalid("process platform must be unix or windows"));
        }
        if self.process(&p.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "owned_process",
                id: p.id.as_str().to_owned(),
            });
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO owned_processes(id,boot_id,state,platform,program,argv_sha256,operation_id,lease_id,actor_id,session_id,project_id,root_id,assignment_id,created_at,updated_at,revision) VALUES(?1,?2,'Launching',?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?13,1)",
        ))?
        .execute(params![
            p.id.as_str(),
            boot.as_str(),
            p.platform,
            p.program,
            p.argv_sha256,
            p.operation_id.as_ref().map(|o| o.0.as_str()),
            p.lease_id.as_ref().map(Id::as_str),
            p.actor.0.as_str(),
            p.session_id.as_ref().map(Id::as_str),
            p.project_id.as_ref().map(|x| x.0.as_str()),
            p.root_id.as_ref().map(|x| x.0.as_str()),
            p.assignment_id.as_ref().map(|x| x.0.as_str()),
            self.now,
        ]))?;
        self.process(&p.id)?
            .ok_or_else(|| corrupt("process vanished inside its transaction"))
    }
    fn update_process(&mut self, p: &StoredProcess, expected: i64) -> Result<(), StorageError> {
        let current = self.process(&p.id)?.ok_or_else(|| StorageError::NotFound {
            kind: "owned_process",
            id: p.id.as_str().to_owned(),
        })?;
        if current.revision != expected || p.revision != expected + 1 {
            return Err(lost("owned_process", &p.id));
        }
        if current.boot_id != p.boot_id
            || current.actor != p.actor
            || current.lease_id != p.lease_id
            || current.argv_sha256 != p.argv_sha256
            || matches!(current.phase, ProcessPhase::Reaped | ProcessPhase::Failed)
            || (current.pid.is_some() && current.pid != p.pid)
        {
            return Err(invalid(
                "a process keeps its boot, owner, lease, argv and identity; a settled one is final",
            ));
        }
        if p.detail.as_ref().is_some_and(|d| d.len() > 1024) {
            return Err(invalid("process detail must be at most 1024 bytes"));
        }
        let n = db(db(self.conn.prepare_cached(
            "UPDATE owned_processes SET state=?2,pid=?3,birth=?4,process_group=?5,exit_code=?6,detail=?7,updated_at=?8,revision=?9 WHERE id=?1 AND revision=?10",
        ))?
        .execute(params![
            p.id.as_str(),
            p.phase.as_str(),
            p.pid,
            p.birth,
            p.group,
            p.exit_code,
            p.detail,
            self.now,
            p.revision,
            expected,
        ]))?;
        if n != 1 {
            return Err(lost("owned_process", &p.id));
        }
        Ok(())
    }
    fn insert_container(&mut self, c: &StoredContainer) -> Result<(), StorageError> {
        if c.revision != 1
            || c.phase != ContainerPhase::Creating
            || c.last_operation_id != c.create_operation_id
            || !matches!(c.engine.as_str(), "docker" | "podman" | "apple")
            || c.image.is_empty()
            || c.image.len() > 4096
            || c.detail.is_some()
            || c.id.as_str().len() > 128
            || !c
                .id
                .as_str()
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
            || !matches!(c.owner_kind.as_str(), "Agent" | "Human" | "Host")
        {
            return Err(invalid("invalid container creation intent"));
        }
        if self.container(&c.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "owned_container",
                id: c.id.as_str().to_owned(),
            });
        }
        if !db(db(self
            .conn
            .prepare_cached("SELECT 1 FROM projects WHERE id=?1 AND lifecycle<>'Deleted'"))?
        .exists([c.project_id.0.as_str()]))?
        {
            return Err(StorageError::NotFound {
                kind: "project",
                id: c.project_id.0.as_str().to_owned(),
            });
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO owned_containers(id,engine,state,image,actor_id,owner_kind,session_id,project_id,root_id,assignment_id,create_operation_id,last_operation_id,detail,revision,created_at,updated_at) VALUES(?1,?2,'Creating',?3,?4,?5,?6,?7,?8,?9,?10,?10,NULL,1,?11,?11)",
        ))?.execute(params![
            c.id.as_str(), c.engine, c.image, c.actor.0.as_str(), c.owner_kind,
            c.session_id.as_ref().map(Id::as_str), c.project_id.0.as_str(),
            c.root_id.as_ref().map(|x| x.0.as_str()), c.assignment_id.as_ref().map(|x| x.0.as_str()),
            c.create_operation_id.0.as_str(), self.now,
        ]))?;
        Ok(())
    }
    fn update_container(&mut self, c: &StoredContainer, expected: i64) -> Result<(), StorageError> {
        let current = self
            .container(&c.id)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "owned_container",
                id: c.id.as_str().to_owned(),
            })?;
        if current.revision != expected || c.revision != expected + 1 {
            return Err(lost("owned_container", &c.id));
        }
        if current.engine != c.engine
            || current.image != c.image
            || current.actor != c.actor
            || current.owner_kind != c.owner_kind
            || current.session_id != c.session_id
            || current.project_id != c.project_id
            || current.root_id != c.root_id
            || current.assignment_id != c.assignment_id
            || current.create_operation_id != c.create_operation_id
            || current.created_at_ms != c.created_at_ms
            || c.detail.as_ref().is_some_and(|x| x.len() > 1024)
        {
            return Err(invalid(
                "container identity changed or detail out of bounds",
            ));
        }
        let n = db(db(self.conn.prepare_cached(
            "UPDATE owned_containers SET state=?2,last_operation_id=?3,detail=?4,revision=?5,updated_at=?6 WHERE id=?1 AND revision=?7",
        ))?.execute(params![c.id.as_str(), c.phase.as_str(), c.last_operation_id.0.as_str(), c.detail, c.revision, self.now, expected]))?;
        if n != 1 {
            return Err(lost("owned_container", &c.id));
        }
        Ok(())
    }
}

fn write_lease(
    conn: &Connection,
    l: &StoredLease,
    sql: &str,
    expected: Option<i64>,
) -> Result<(), StorageError> {
    let mut s = db(conn.prepare_cached(sql))?;
    let base = rusqlite::params![
        l.id.as_str(),
        l.resource_id.as_str(),
        l.phase.as_str(),
        l.end_reason.map(LeaseEndReason::as_str),
        l.owner.0.as_str(),
        l.holder_key,
        l.session_id.as_ref().map(Id::as_str),
        l.project_id.as_ref().map(|p| p.0.as_str()),
        l.root_id.as_ref().map(|r| r.0.as_str()),
        l.assignment_id.as_ref().map(|a| a.0.as_str()),
        l.boot_id.as_str(),
        l.purpose,
        l.priority,
        l.units,
        l.lease_ms,
        l.queue_seq,
        l.fence,
        l.requested_at_ms,
        l.granted_at_ms,
        l.renewed_at_ms,
        l.expires_at_ms,
        l.ended_at_ms,
        l.unreachable_since_ms,
        l.revision,
    ];
    let n = match expected {
        None => db(s.execute(base))?,
        Some(e) => {
            let mut all: Vec<&dyn rusqlite::ToSql> = base.to_vec();
            all.push(&e);
            db(s.execute(all.as_slice()))?
        }
    };
    if n != 1 {
        return Err(lost("resource_lease", &l.id));
    }
    Ok(())
}
