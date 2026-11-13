//! SQLite planned work records: `work_items`, `work_repositories`, `work_dependencies`,
//! `workflow_statuses` and `feature_plans` (migration 1) with the side tables `work_details`,
//! `work_queues`, `plan_proposals` and `feature_readiness` (migration 16).
//!
//! A work row without its detail row reads with the migration's defaults, so an item recorded
//! before migration 16 (or by a fixture) stays readable. Text search compares a search key the
//! service folded with a needle it folded the same way (`instr`, exact), so the comparison is
//! symmetric whatever the script (SQLite's `lower()`/`LIKE` fold only ASCII).
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, ProjectId, Revision, RevisionDimension};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde_json::Value;

fn invalid(m: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(m.into())
}
fn corrupt(m: impl Into<String>) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, m)
}
fn id_of(s: String) -> Result<Id, StorageError> {
    Id::new(s).map_err(|e| corrupt(format!("stored identifier: {e}")))
}
fn limit(l: u16) -> Result<i64, StorageError> {
    if l == 0 || l > MAX_PAGE {
        return Err(invalid(format!("limit must be 1..={MAX_PAGE}")));
    }
    Ok(i64::from(l))
}
fn revision(n: i64) -> Result<Revision, StorageError> {
    Revision::new(n).map_err(|_| corrupt("stored revision is not positive"))
}
fn expected_rev(n: i64) -> Result<Revision, StorageError> {
    Revision::new(n).map_err(|_| invalid("expected revision is positive"))
}
fn json_of(s: &str) -> Result<Value, StorageError> {
    serde_json::from_str(s).map_err(|e| corrupt(format!("stored JSON: {e}")))
}
fn text_of(v: &Value) -> Result<String, StorageError> {
    if !v.is_object() {
        return Err(invalid("a stored body is a JSON object"));
    }
    serde_json::to_string(v).map_err(|e| invalid(e.to_string()))
}
fn ids(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Vec<Id>, StorageError> {
    let mut st = db(conn.prepare_cached(sql))?;
    let rows =
        db(db(st.query_map(p, |r| r.get::<_, String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter().map(id_of).collect()
}

// ---------------------------------------------------------------------------------------------
// Work items

const WORK_COLUMNS: &str = "w.id,w.project_id,w.kind,w.parent_feature_id,w.status_id,w.title,w.priority,w.manual_order,w.revision,w.scope_revision,w.created_at,COALESCE(d.updated_at,w.created_at),d.created_by,d.updated_by,COALESCE(d.search_key,''),d.caller_ref,d.completion,w.brief_json";
const WORK_FROM: &str = "work_items w LEFT JOIN work_details d ON d.work_id=w.id";

type WorkRaw = (
    (
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
    ),
    (i64, i64, i64, i64, i64, i64),
    (
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        String,
    ),
);
fn work_row(r: &Row<'_>) -> rusqlite::Result<WorkRaw> {
    Ok((
        (
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ),
        (
            r.get(6)?,
            r.get(7)?,
            r.get(8)?,
            r.get(9)?,
            r.get(10)?,
            r.get(11)?,
        ),
        (
            r.get(12)?,
            r.get(13)?,
            r.get(14)?,
            r.get(15)?,
            r.get(16)?,
            r.get(17)?,
        ),
    ))
}
fn work_of(raw: WorkRaw) -> Result<StoredWork, StorageError> {
    let (
        (id, project, kind, parent, status, title),
        (priority, manual_order, rev, scope, created, updated),
        (created_by, updated_by, search_key, caller_ref, completion, brief),
    ) = raw;
    let kind = if kind == "issue" { TASK.into() } else { kind };
    Ok(StoredWork {
        id: id_of(id)?,
        project_id: ProjectId(id_of(project)?),
        kind,
        parent_feature_id: parent.map(id_of).transpose()?,
        status_id: status.map(id_of).transpose()?,
        title,
        priority,
        manual_order,
        revision: rev,
        scope_revision: scope,
        created_at_ms: created,
        updated_at_ms: updated,
        created_by: created_by.map(id_of).transpose()?.map(ActorId),
        updated_by: updated_by.map(id_of).transpose()?.map(ActorId),
        search_key,
        caller_ref: caller_ref.map(id_of).transpose()?,
        completion,
        brief: json_of(&brief)?,
    })
}
fn one_work(conn: &Connection, id: &Id) -> Result<Option<StoredWork>, StorageError> {
    let raw = db(db(conn.prepare_cached(&format!(
        "SELECT {WORK_COLUMNS} FROM {WORK_FROM} WHERE w.id=?1"
    )))?
    .query_row([id.as_str()], work_row)
    .optional())?;
    raw.map(work_of).transpose()
}

fn list_works(
    conn: &Connection,
    f: &WorkFilter,
    after: Option<&WorkPosition>,
    n: u16,
) -> Result<Vec<StoredWork>, StorageError> {
    let n = limit(n)?;
    if f.status_ids.len() > 128 || f.repository_ids.len() > 128 {
        return Err(invalid("at most 128 statuses and 128 repositories"));
    }
    let mut sql = format!("SELECT {WORK_COLUMNS} FROM {WORK_FROM} WHERE w.project_id=?");
    let mut values: Vec<Sql> = vec![Sql::Text(f.project.0.as_str().to_owned())];
    match &f.selection {
        WorkSelection::Features => sql.push_str(" AND w.kind='feature'"),
        WorkSelection::Standalone => {
            sql.push_str(" AND w.kind<>'feature' AND w.parent_feature_id IS NULL")
        }
        WorkSelection::Tasks => sql.push_str(" AND w.kind<>'feature'"),
        WorkSelection::FeatureTasks(fid) => {
            sql.push_str(" AND w.kind<>'feature' AND w.parent_feature_id=?");
            values.push(Sql::Text(fid.as_str().to_owned()));
        }
    }
    if !f.status_ids.is_empty() {
        sql.push_str(&format!(
            " AND w.status_id IN ({})",
            vec!["?"; f.status_ids.len()].join(",")
        ));
        values.extend(
            f.status_ids
                .iter()
                .map(|s| Sql::Text(s.as_str().to_owned())),
        );
    }
    if !f.repository_ids.is_empty() {
        sql.push_str(&format!(
            " AND EXISTS(SELECT 1 FROM work_repositories r WHERE r.work_id=w.id AND r.repository_id IN ({}))",
            vec!["?"; f.repository_ids.len()].join(",")
        ));
        values.extend(
            f.repository_ids
                .iter()
                .map(|s| Sql::Text(s.as_str().to_owned())),
        );
    }
    if let Some(t) = &f.text {
        sql.push_str(" AND instr(COALESCE(d.search_key,''),?)>0");
        values.push(Sql::Text(t.clone()));
    }
    if let Some(a) = after {
        sql.push_str(
            " AND (w.priority<? OR (w.priority=? AND (w.manual_order>? OR (w.manual_order=? AND (w.created_at>? OR (w.created_at=? AND w.id>?))))))",
        );
        values.extend([
            Sql::Integer(a.priority),
            Sql::Integer(a.priority),
            Sql::Integer(a.manual_order),
            Sql::Integer(a.manual_order),
            Sql::Integer(a.created_at_ms),
            Sql::Integer(a.created_at_ms),
            Sql::Text(a.id.as_str().to_owned()),
        ]);
    }
    sql.push_str(" ORDER BY w.priority DESC,w.manual_order,w.created_at,w.id LIMIT ?");
    values.push(Sql::Integer(n));
    let mut st = db(conn.prepare(&sql))?;
    let rows =
        db(db(st.query_map(params_from_iter(values), work_row))?
            .collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter().map(work_of).collect()
}

fn check_work(w: &StoredWork) -> Result<(), StorageError> {
    if w.kind != FEATURE && w.kind != TASK {
        return Err(invalid("a work item is a feature or a task"));
    }
    if w.kind == FEATURE && w.parent_feature_id.is_some() {
        return Err(invalid("a feature has no feature membership"));
    }
    if w.title.is_empty() || w.title.chars().count() > 256 {
        return Err(invalid("a work title is 1..=256 characters"));
    }
    if w.revision < 1 || w.scope_revision < 1 {
        return Err(invalid("work revisions are positive"));
    }
    if i32::try_from(w.priority).is_err() {
        return Err(invalid("a priority is a 32-bit integer"));
    }
    if w.completion
        .as_deref()
        .is_some_and(|c| c != MANUAL && c != VERIFIED)
    {
        return Err(invalid("completion is Manual or Verified"));
    }
    if !w.brief.is_object() {
        return Err(invalid("a work brief is a JSON object"));
    }
    Ok(())
}
fn check_parent(conn: &Connection, w: &StoredWork) -> Result<(), StorageError> {
    if let Some(p) = &w.parent_feature_id {
        let ok: bool = db(db(conn.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM work_items WHERE id=?1 AND project_id=?2 AND kind='feature')",
        ))?
        .query_row(params![p.as_str(), w.project_id.0.as_str()], |r| r.get(0)))?;
        if !ok {
            return Err(invalid("a card's parent is a feature of its project"));
        }
    }
    if let Some(s) = &w.status_id {
        let ok: bool = db(db(conn.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM workflow_statuses WHERE id=?1 AND project_id=?2)",
        ))?
        .query_row(params![s.as_str(), w.project_id.0.as_str()], |r| r.get(0)))?;
        if !ok {
            return Err(invalid("a work status is a status of its project"));
        }
    }
    Ok(())
}
fn write_details(conn: &Connection, w: &StoredWork) -> Result<(), StorageError> {
    if let Some(r) = &w.caller_ref {
        let other: Option<String> = db(db(conn.prepare_cached(
            "SELECT work_id FROM work_details WHERE project_id=?1 AND caller_ref=?2",
        ))?
        .query_row(params![w.project_id.0.as_str(), r.as_str()], |r| r.get(0))
        .optional())?;
        if other.is_some_and(|o| o != w.id.as_str()) {
            return Err(StorageError::Duplicate {
                kind: "caller_ref",
                id: r.as_str().to_owned(),
            });
        }
    }
    db(db(conn.prepare_cached(
        "INSERT INTO work_details(work_id,project_id,search_key,caller_ref,completion,created_by,updated_by,updated_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(work_id) DO UPDATE SET search_key=excluded.search_key,caller_ref=excluded.caller_ref,
         completion=excluded.completion,created_by=excluded.created_by,updated_by=excluded.updated_by,
         updated_at=excluded.updated_at",
    ))?
    .execute(params![
        w.id.as_str(),
        w.project_id.0.as_str(),
        w.search_key,
        w.caller_ref.as_ref().map(Id::as_str),
        w.completion,
        w.created_by.as_ref().map(|a| a.0.as_str()),
        w.updated_by.as_ref().map(|a| a.0.as_str()),
        w.updated_at_ms
    ]))?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Plans, proposals, readiness

fn plan_of(conn: &Connection, feature: &Id) -> Result<Option<StoredFeaturePlan>, StorageError> {
    type Raw = (
        String,
        i64,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<i64>,
    );
    let raw: Option<Raw> = db(db(conn.prepare_cached(
        "SELECT feature_id,revision,approved_plan_json,approved_by,approved_at,pending_plan_json,pending_revision FROM feature_plans WHERE feature_id=?1",
    ))?
    .query_row([feature.as_str()], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
        ))
    })
    .optional())?;
    raw.map(|(f, rev, approved, by, at, pending, prev)| {
        Ok(StoredFeaturePlan {
            feature_id: id_of(f)?,
            revision: rev,
            approved: approved.as_deref().map(json_of).transpose()?,
            approved_by: by.map(id_of).transpose()?.map(ActorId),
            approved_at_ms: at,
            pending: pending.as_deref().map(json_of).transpose()?,
            pending_revision: prev,
        })
    })
    .transpose()
}

const PROPOSAL_COLUMNS: &str = "id,feature_id,revision,plan_revision,plan_digest,state,requested_by,requested_at,decided_by,decided_at,summary,reviewed_snapshot_json";
type ProposalRaw = (
    (String, String, i64, i64, String, String),
    (
        String,
        i64,
        Option<String>,
        Option<i64>,
        String,
        Option<String>,
    ),
);
fn proposal_row(r: &Row<'_>) -> rusqlite::Result<ProposalRaw> {
    Ok((
        (
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
        ),
        (
            r.get(6)?,
            r.get(7)?,
            r.get(8)?,
            r.get(9)?,
            r.get(10)?,
            r.get(11)?,
        ),
    ))
}
fn proposal_of(raw: ProposalRaw) -> Result<StoredProposal, StorageError> {
    let (
        (id, feature, rev, plan_rev, digest, state),
        (by, at, decided_by, decided_at, summary, snapshot),
    ) = raw;
    Ok(StoredProposal {
        id: id_of(id)?,
        feature_id: id_of(feature)?,
        revision: rev,
        plan_revision: plan_rev,
        plan_digest: digest,
        state,
        requested_by: ActorId(id_of(by)?),
        requested_at_ms: at,
        decided_by: decided_by.map(id_of).transpose()?.map(ActorId),
        decided_at_ms: decided_at,
        summary,
        reviewed_snapshot: snapshot.as_deref().map(json_of).transpose()?,
    })
}

macro_rules! work_reads {
    ($t:ty) => {
        impl WorkRead for $t {
            fn allocation_replay(&self,key:&str)->Result<Option<StoredAllocationReplay>,StorageError>{super::allocations::replay(self.conn,key)}
            fn allocation_claim(&self,id:&Id)->Result<Option<StoredAllocationClaim>,StorageError>{super::allocations::one(self.conn,id)}
            fn allocation_claims(&self,project:&ProjectId,after:Option<&Id>,limit:u16)->Result<Vec<StoredAllocationClaim>,StorageError>{super::allocations::page(self.conn,project,after,limit)}
            fn allocation_by_invocation(&self,namespace:&Id,invocation:&Id)->Result<Option<StoredAllocationClaim>,StorageError>{super::allocations::invocation(self.conn,namespace,invocation)}
            fn work(&self, id: &Id) -> Result<Option<StoredWork>, StorageError> {
                one_work(self.conn, id)
            }
            fn works(
                &self,
                filter: &WorkFilter,
                after: Option<&WorkPosition>,
                n: u16,
            ) -> Result<Vec<StoredWork>, StorageError> {
                list_works(self.conn, filter, after, n)
            }
            fn work_repositories(&self, id: &Id) -> Result<Vec<Id>, StorageError> {
                ids(
                    self.conn,
                    "SELECT repository_id FROM work_repositories WHERE work_id=?1 ORDER BY repository_id",
                    [id.as_str()],
                )
            }
            fn prerequisites(&self, id: &Id) -> Result<Vec<Id>, StorageError> {
                ids(
                    self.conn,
                    "SELECT DISTINCT prerequisite_id FROM work_dependencies WHERE work_id=?1 ORDER BY prerequisite_id",
                    [id.as_str()],
                )
            }
            fn dependents(&self, id: &Id) -> Result<Vec<Id>, StorageError> {
                ids(
                    self.conn,
                    "SELECT DISTINCT work_id FROM work_dependencies WHERE prerequisite_id=?1 ORDER BY work_id",
                    [id.as_str()],
                )
            }
            fn open_prerequisites(&self, id: &Id) -> Result<Vec<Id>, StorageError> {
                open_prerequisites(self.conn, id)
            }
            fn queue_revision(
                &self,
                project: &ProjectId,
                queue: &str,
            ) -> Result<i64, StorageError> {
                queue_revision(self.conn, project, queue)
            }
            fn next_rank(&self, project: &ProjectId, queue: &str) -> Result<i64, StorageError> {
                let max: Option<i64> = if queue == PROJECT_QUEUE {
                    db(db(self.conn.prepare_cached(
                        "SELECT max(manual_order) FROM work_items WHERE project_id=?1 AND (kind='feature' OR parent_feature_id IS NULL)",
                    ))?
                    .query_row([project.0.as_str()], |r| r.get(0)))?
                } else if let Some(f) = queue.strip_prefix("feature:") {
                    db(db(self.conn.prepare_cached(
                        "SELECT max(manual_order) FROM work_items WHERE project_id=?1 AND kind<>'feature' AND parent_feature_id=?2",
                    ))?
                    .query_row(params![project.0.as_str(), f], |r| r.get(0)))?
                } else {
                    return Err(invalid("not a ranked queue"));
                };
                Ok(max.map_or(1, |m| m + 1))
            }
            fn statuses(&self, project: &ProjectId) -> Result<Vec<StoredStatus>, StorageError> {
                let mut st = db(self.conn.prepare_cached(
                    "SELECT id,project_id,name,semantic,sort_order,revision FROM workflow_statuses WHERE project_id=?1 ORDER BY sort_order,id",
                ))?;
                let rows = db(db(st.query_map([project.0.as_str()], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                }))?
                .collect::<rusqlite::Result<Vec<_>>>())?;
                rows.into_iter()
                    .map(|(id, p, name, semantic, order, rev)| {
                        Ok(StoredStatus {
                            id: id_of(id)?,
                            project_id: ProjectId(id_of(p)?),
                            name,
                            semantic,
                            sort_order: order,
                            revision: rev,
                        })
                    })
                    .collect()
            }
            fn by_caller_ref(
                &self,
                project: &ProjectId,
                caller_ref: &Id,
            ) -> Result<Option<Id>, StorageError> {
                let found: Option<String> = db(db(self.conn.prepare_cached(
                    "SELECT work_id FROM work_details WHERE project_id=?1 AND caller_ref=?2",
                ))?
                .query_row(params![project.0.as_str(), caller_ref.as_str()], |r| r.get(0))
                .optional())?;
                found.map(id_of).transpose()
            }
            fn feature_plan(
                &self,
                feature: &Id,
            ) -> Result<Option<StoredFeaturePlan>, StorageError> {
                plan_of(self.conn, feature)
            }
            fn proposal(&self, id: &Id) -> Result<Option<StoredProposal>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(&format!(
                    "SELECT {PROPOSAL_COLUMNS} FROM plan_proposals WHERE id=?1"
                )))?
                .query_row([id.as_str()], proposal_row)
                .optional())?;
                raw.map(proposal_of).transpose()
            }
            fn latest_proposal(
                &self,
                feature: &Id,
            ) -> Result<Option<StoredProposal>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(&format!(
                    "SELECT {PROPOSAL_COLUMNS} FROM plan_proposals WHERE feature_id=?1 ORDER BY requested_at DESC, rowid DESC LIMIT 1"
                )))?
                .query_row([feature.as_str()], proposal_row)
                .optional())?;
                raw.map(proposal_of).transpose()
            }
            fn readiness(&self, feature: &Id) -> Result<Option<StoredReadiness>, StorageError> {
                readiness_of(self.conn, feature)
            }
        }
    };
}
work_reads!(Snapshot<'_>);
work_reads!(Unit<'_>);

/// Prerequisites of `id` that are not in a Done status with verified completion.
pub(crate) fn open_prerequisites(conn: &Connection, id: &Id) -> Result<Vec<Id>, StorageError> {
    ids(
        conn,
        "SELECT DISTINCT p.id FROM work_dependencies dep
         JOIN work_items p ON p.id=dep.prerequisite_id
         LEFT JOIN work_details d ON d.work_id=p.id
         LEFT JOIN workflow_statuses s ON s.id=p.status_id AND s.project_id=p.project_id
         WHERE dep.work_id=?1 AND NOT (COALESCE(s.semantic,'')='Done' AND COALESCE(d.completion,'')='Verified')
         ORDER BY p.id",
        [id.as_str()],
    )
}
fn queue_revision(
    conn: &Connection,
    project: &ProjectId,
    queue: &str,
) -> Result<i64, StorageError> {
    let r: Option<i64> = db(db(conn.prepare_cached(
        "SELECT revision FROM work_queues WHERE project_id=?1 AND queue_key=?2",
    ))?
    .query_row(params![project.0.as_str(), queue], |r| r.get(0))
    .optional())?;
    Ok(r.unwrap_or(1))
}
pub(crate) fn readiness_of(
    conn: &Connection,
    feature: &Id,
) -> Result<Option<StoredReadiness>, StorageError> {
    let raw: Option<(String, i64, String)> = db(db(conn.prepare_cached(
        "SELECT feature_id,revision,body_json FROM feature_readiness WHERE feature_id=?1",
    ))?
    .query_row([feature.as_str()], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })
    .optional())?;
    raw.map(|(f, rev, body)| {
        Ok(StoredReadiness {
            feature_id: id_of(f)?,
            revision: rev,
            body: json_of(&body)?,
        })
    })
    .transpose()
}

fn plan_conflict(id: &Id, expected: i64, current: i64) -> Result<StorageError, StorageError> {
    Ok(StorageError::conflict(
        RevisionDimension::Plan,
        "plan",
        id.clone(),
        expected_rev(expected)?,
        revision(current)?,
    ))
}

impl WorkRepository for Unit<'_> {
    fn insert_allocation_replay(
        &mut self,
        replay: &StoredAllocationReplay,
    ) -> Result<(), StorageError> {
        super::allocations::insert_replay(self, replay)
    }
    fn insert_allocation_claim(
        &mut self,
        claim: &StoredAllocationClaim,
    ) -> Result<(), StorageError> {
        super::allocations::insert(self, claim)
    }
    fn update_allocation_claim(
        &mut self,
        claim: &StoredAllocationClaim,
        expected: i64,
    ) -> Result<(), StorageError> {
        super::allocations::update(self, claim, expected)
    }
    fn update_status(&mut self, s: &StoredStatus, expected: i64) -> Result<(), StorageError> {
        let current = self
            .statuses(&s.project_id)?
            .into_iter()
            .find(|v| v.id == s.id)
            .ok_or_else(|| StorageError::NotFound {
                kind: "workflow_status",
                id: s.id.as_str().into(),
            })?;
        if current.revision != expected {
            return Err(StorageError::conflict(
                RevisionDimension::Workflow,
                "workflow_status",
                s.id.clone(),
                expected_rev(expected)?,
                revision(current.revision)?,
            ));
        }
        if s.revision
            != expected
                .checked_add(1)
                .ok_or_else(|| invalid("status revision overflow"))?
            || s.semantic != current.semantic
            || s.name.is_empty()
            || s.name.chars().count() > 256
        {
            return Err(invalid(
                "status update preserves semantic and advances revision once",
            ));
        }
        db(self.conn.execute("UPDATE workflow_statuses SET name=?2,sort_order=?3,revision=?4 WHERE id=?1 AND project_id=?5 AND revision=?6",params![s.id.as_str(),s.name,s.sort_order,s.revision,s.project_id.0.as_str(),expected]))?;
        Ok(())
    }
    fn delete_status(
        &mut self,
        project: &ProjectId,
        id: &Id,
        expected: i64,
    ) -> Result<(), StorageError> {
        let current = self
            .statuses(project)?
            .into_iter()
            .find(|v| &v.id == id)
            .ok_or_else(|| StorageError::NotFound {
                kind: "workflow_status",
                id: id.as_str().into(),
            })?;
        if current.revision != expected {
            return Err(StorageError::conflict(
                RevisionDimension::Workflow,
                "workflow_status",
                id.clone(),
                expected_rev(expected)?,
                revision(current.revision)?,
            ));
        }
        let occupied: bool = db(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_items WHERE status_id=?1)",
            [id.as_str()],
            |row| row.get(0),
        ))?;
        if occupied {
            return Err(invalid(
                "occupied status must be reassigned atomically before removal",
            ));
        }
        db(self.conn.execute(
            "DELETE FROM workflow_statuses WHERE id=?1 AND project_id=?2 AND revision=?3",
            params![id.as_str(), project.0.as_str(), expected],
        ))?;
        Ok(())
    }
    fn insert_work(&mut self, w: &StoredWork) -> Result<(), StorageError> {
        check_work(w)?;
        if w.revision != 1 {
            return Err(invalid("a new work item starts at revision 1"));
        }
        if one_work(self.conn, &w.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "work",
                id: w.id.as_str().to_owned(),
            });
        }
        check_parent(self.conn, w)?;
        let brief = text_of(&w.brief)?;
        db(db(self.conn.prepare_cached(
            "INSERT INTO work_items(id,project_id,kind,parent_feature_id,associated_feature_id,status_id,title,priority,manual_order,revision,scope_revision,created_at,brief_json)
             VALUES(?1,?2,?3,?4,NULL,?5,?6,?7,?8,1,?9,?10,?11)",
        ))?
        .execute(params![
            w.id.as_str(),
            w.project_id.0.as_str(),
            w.kind,
            w.parent_feature_id.as_ref().map(Id::as_str),
            w.status_id.as_ref().map(Id::as_str),
            w.title,
            w.priority,
            w.manual_order,
            w.scope_revision,
            w.created_at_ms,
            brief
        ]))?;
        write_details(self.conn, w)
    }
    fn update_work(&mut self, w: &StoredWork, expected: i64) -> Result<(), StorageError> {
        check_work(w)?;
        let Some(current) = one_work(self.conn, &w.id)? else {
            return Err(StorageError::NotFound {
                kind: "work",
                id: w.id.as_str().to_owned(),
            });
        };
        if current.revision != expected {
            return Err(StorageError::conflict(
                RevisionDimension::Work,
                "work",
                w.id.clone(),
                expected_rev(expected)?,
                revision(current.revision)?,
            ));
        }
        if w.revision != expected + 1
            || w.kind != current.kind
            || w.project_id != current.project_id
            || w.created_at_ms != current.created_at_ms
            || w.scope_revision < current.scope_revision
        {
            return Err(invalid(
                "a work update advances its revision by one and keeps its kind, project and creation",
            ));
        }
        check_parent(self.conn, w)?;
        let brief = text_of(&w.brief)?;
        let n = db(db(self.conn.prepare_cached(
            "UPDATE work_items SET parent_feature_id=?1,status_id=?2,title=?3,priority=?4,manual_order=?5,revision=?6,scope_revision=?7,brief_json=?8 WHERE id=?9 AND revision=?10",
        ))?
        .execute(params![
            w.parent_feature_id.as_ref().map(Id::as_str),
            w.status_id.as_ref().map(Id::as_str),
            w.title,
            w.priority,
            w.manual_order,
            w.revision,
            w.scope_revision,
            brief,
            w.id.as_str(),
            expected
        ]))?;
        if n != 1 {
            return Err(corrupt("work item changed inside its transaction"));
        }
        write_details(self.conn, w)
    }
    fn set_rank(&mut self, id: &Id, rank: i64) -> Result<(), StorageError> {
        let n = db(db(self
            .conn
            .prepare_cached("UPDATE work_items SET manual_order=?1 WHERE id=?2"))?
        .execute(params![rank, id.as_str()]))?;
        if n != 1 {
            return Err(StorageError::NotFound {
                kind: "work",
                id: id.as_str().to_owned(),
            });
        }
        Ok(())
    }
    fn set_work_repositories(&mut self, id: &Id, repos: &[Id]) -> Result<(), StorageError> {
        let mut seen = std::collections::BTreeSet::new();
        if repos.iter().any(|r| !seen.insert(r.as_str())) {
            return Err(invalid("a work item names each repository once"));
        }
        db(db(self
            .conn
            .prepare_cached("DELETE FROM work_repositories WHERE work_id=?1"))?
        .execute([id.as_str()]))?;
        for r in repos {
            db(db(self.conn.prepare_cached(
                "INSERT INTO work_repositories(work_id,repository_id) VALUES(?1,?2)",
            ))?
            .execute(params![id.as_str(), r.as_str()]))?;
        }
        Ok(())
    }
    fn set_prerequisites(&mut self, id: &Id, prereqs: &[Id]) -> Result<(), StorageError> {
        let mut seen = std::collections::BTreeSet::new();
        for p in prereqs {
            if p == id {
                return Err(invalid("an item cannot block itself"));
            }
            if !seen.insert(p.as_str()) {
                return Err(invalid("a blocking link is recorded once"));
            }
        }
        db(db(self
            .conn
            .prepare_cached("DELETE FROM work_dependencies WHERE work_id=?1"))?
        .execute([id.as_str()]))?;
        for p in prereqs {
            db(db(self.conn.prepare_cached(
                "INSERT INTO work_dependencies(work_id,prerequisite_id,condition_kind) VALUES(?1,?2,'blocks')",
            ))?
            .execute(params![id.as_str(), p.as_str()]))?;
        }
        Ok(())
    }
    fn bump_queue(&mut self, project: &ProjectId, queue: &str) -> Result<i64, StorageError> {
        if queue.is_empty() || queue.chars().count() > 160 {
            return Err(invalid("a queue key is 1..=160 characters"));
        }
        let next = queue_revision(self.conn, project, queue)? + 1;
        db(db(self.conn.prepare_cached(
            "INSERT INTO work_queues(project_id,queue_key,revision) VALUES(?1,?2,?3)
             ON CONFLICT(project_id,queue_key) DO UPDATE SET revision=excluded.revision",
        ))?
        .execute(params![project.0.as_str(), queue, next]))?;
        Ok(next)
    }
    fn insert_status(&mut self, s: &StoredStatus) -> Result<(), StorageError> {
        if s.name.is_empty() || s.revision < 1 {
            return Err(invalid("a status has a name and a positive revision"));
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO workflow_statuses(id,project_id,name,semantic,sort_order,revision) VALUES(?1,?2,?3,?4,?5,?6)",
        ))?
        .execute(params![
            s.id.as_str(),
            s.project_id.0.as_str(),
            s.name,
            s.semantic,
            s.sort_order,
            s.revision
        ]))?;
        Ok(())
    }
    fn put_feature_plan(
        &mut self,
        p: &StoredFeaturePlan,
        expected: Option<i64>,
    ) -> Result<(), StorageError> {
        let approved = p.approved.as_ref().map(text_of).transpose()?;
        let pending = p.pending.as_ref().map(text_of).transpose()?;
        let current = plan_of(self.conn, &p.feature_id)?;
        match (expected, &current) {
            (None, Some(c)) => return Err(plan_conflict(&p.feature_id, 1, c.revision)?),
            (Some(e), None) => {
                return Err(StorageError::NotFound {
                    kind: "plan",
                    id: format!("{} at {e}", p.feature_id.as_str()),
                });
            }
            (Some(e), Some(c)) if c.revision != e => {
                return Err(plan_conflict(&p.feature_id, e, c.revision)?);
            }
            _ => {}
        }
        if p.revision != expected.map_or(1, |e| e + 1) {
            return Err(invalid("a plan change advances its revision by one"));
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO feature_plans(feature_id,revision,approved_plan_json,approved_by,approved_at,pending_plan_json,pending_revision)
             VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(feature_id) DO UPDATE SET revision=excluded.revision,approved_plan_json=excluded.approved_plan_json,
             approved_by=excluded.approved_by,approved_at=excluded.approved_at,pending_plan_json=excluded.pending_plan_json,
             pending_revision=excluded.pending_revision",
        ))?
        .execute(params![
            p.feature_id.as_str(),
            p.revision,
            approved,
            p.approved_by.as_ref().map(|a| a.0.as_str()),
            p.approved_at_ms,
            pending,
            p.pending_revision
        ]))?;
        Ok(())
    }
    fn put_proposal(
        &mut self,
        p: &StoredProposal,
        expected: Option<i64>,
    ) -> Result<(), StorageError> {
        if !PROPOSAL_STATES.contains(&p.state.as_str()) {
            return Err(invalid("unknown approval state"));
        }
        if let Some(stored) = WorkRead::proposal(self, &p.id)? {
            let mut content = p.clone();
            content.revision = stored.revision;
            content.state = stored.state.clone();
            content.decided_by = stored.decided_by.clone();
            content.decided_at_ms = stored.decided_at_ms;
            if content != stored {
                return Err(invalid(
                    "plan proposal reviewed content and binding are immutable",
                ));
            }
        } else if p.reviewed_snapshot.is_none() {
            return Err(invalid(
                "new plan proposals retain their exact reviewed snapshot",
            ));
        }
        let current: Option<i64> = db(db(self
            .conn
            .prepare_cached("SELECT revision FROM plan_proposals WHERE id=?1"))?
        .query_row([p.id.as_str()], |r| r.get(0))
        .optional())?;
        match (expected, current) {
            (None, None) => {
                if p.revision != 1 {
                    return Err(invalid("a new proposal starts at revision 1"));
                }
                db(db(self.conn.prepare_cached(&format!(
                    "INSERT INTO plan_proposals({PROPOSAL_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)"
                )))?
                .execute(params![
                    p.id.as_str(),
                    p.feature_id.as_str(),
                    p.revision,
                    p.plan_revision,
                    p.plan_digest,
                    p.state,
                    p.requested_by.0.as_str(),
                    p.requested_at_ms,
                    p.decided_by.as_ref().map(|a| a.0.as_str()),
                    p.decided_at_ms,
                    p.summary,
                    p.reviewed_snapshot.as_ref().map(text_of).transpose()?
                ]))?;
                Ok(())
            }
            (Some(e), Some(c)) if c == e && p.revision == e + 1 => {
                db(db(self.conn.prepare_cached(
                    "UPDATE plan_proposals SET revision=?1,state=?2,decided_by=?3,decided_at=?4 WHERE id=?5 AND revision=?6",
                ))?
                .execute(params![
                    p.revision,
                    p.state,
                    p.decided_by.as_ref().map(|a| a.0.as_str()),
                    p.decided_at_ms,
                    p.id.as_str(),
                    e
                ]))?;
                Ok(())
            }
            (Some(e), Some(c)) if c != e => Err(StorageError::conflict(
                RevisionDimension::Review,
                "proposal",
                p.id.clone(),
                expected_rev(e)?,
                revision(c)?,
            )),
            (None, Some(_)) => Err(StorageError::Duplicate {
                kind: "proposal",
                id: p.id.as_str().to_owned(),
            }),
            (Some(_), None) => Err(StorageError::NotFound {
                kind: "proposal",
                id: p.id.as_str().to_owned(),
            }),
            _ => Err(invalid("a proposal change advances its revision by one")),
        }
    }
    fn put_readiness(
        &mut self,
        r: &StoredReadiness,
        expected: Option<i64>,
    ) -> Result<(), StorageError> {
        let body = text_of(&r.body)?;
        let current = readiness_of(self.conn, &r.feature_id)?.map(|c| c.revision);
        match (expected, current) {
            (None, None) if r.revision == 1 => {}
            (Some(e), Some(c)) if c == e && r.revision == e + 1 => {}
            (Some(e), Some(c)) if c != e => {
                return Err(StorageError::conflict(
                    RevisionDimension::Plan,
                    "readiness",
                    r.feature_id.clone(),
                    expected_rev(e)?,
                    revision(c)?,
                ));
            }
            (None, Some(c)) => {
                return Err(StorageError::conflict(
                    RevisionDimension::Plan,
                    "readiness",
                    r.feature_id.clone(),
                    expected_rev(1)?,
                    revision(c)?,
                ));
            }
            _ => return Err(invalid("a readiness change advances its revision by one")),
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO feature_readiness(feature_id,revision,body_json) VALUES(?1,?2,?3)
             ON CONFLICT(feature_id) DO UPDATE SET revision=excluded.revision,body_json=excluded.body_json",
        ))?
        .execute(params![r.feature_id.as_str(), r.revision, body]))?;
        Ok(())
    }
}
