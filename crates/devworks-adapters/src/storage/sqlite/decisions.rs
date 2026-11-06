//! Decision inbox references, immutable human review evidence and the private resume outbox.
use super::conn::db;
use super::repos::{Snapshot, Unit, parse_principal, principal_name};
use devworks_core::context::PrincipalKind;
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, ProjectId, Revision, RevisionDimension, RootId};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde_json::{Map, Value};

fn invalid(s: &str) -> StorageError {
    StorageError::InvalidRecord(s.into())
}
fn corrupt(s: &str) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, s)
}
fn text(v: &Value, k: &str) -> Result<String, StorageError> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| corrupt("missing stored decision text"))
}
fn optional_text(v: &Value, k: &str) -> Result<Option<String>, StorageError> {
    if v.get(k).is_none_or(Value::is_null) {
        Ok(None)
    } else {
        text(v, k).map(Some)
    }
}
fn num(v: &Value, k: &str) -> Result<i64, StorageError> {
    v.get(k)
        .and_then(Value::as_i64)
        .ok_or_else(|| corrupt("missing stored decision counter"))
}
fn optional_num(v: &Value, k: &str) -> Result<Option<i64>, StorageError> {
    if v.get(k).is_none_or(Value::is_null) {
        Ok(None)
    } else {
        num(v, k).map(Some)
    }
}
fn id(v: String) -> Result<Id, StorageError> {
    Id::new(v).map_err(|_| corrupt("invalid stored decision identifier"))
}
fn field_id(v: &Value, k: &str) -> Result<Id, StorageError> {
    id(text(v, k)?)
}
fn optional_id(v: &Value, k: &str) -> Result<Option<Id>, StorageError> {
    optional_text(v, k)?.map(id).transpose()
}
fn json(v: &Value, k: &str) -> Result<Value, StorageError> {
    serde_json::from_str(&text(v, k)?).map_err(|_| corrupt("invalid stored decision JSON"))
}
fn optional_json(v: &Value, k: &str) -> Result<Option<Value>, StorageError> {
    optional_text(v, k)?
        .map(|s| serde_json::from_str(&s).map_err(|_| corrupt("invalid stored decision JSON")))
        .transpose()
}
fn document(row: &Row<'_>) -> rusqlite::Result<Value> {
    let mut value = Map::new();
    for (index, name) in row.as_ref().column_names().enumerate() {
        let field = match row.get::<_, Sql>(index)? {
            Sql::Null => Value::Null,
            Sql::Integer(v) => Value::from(v),
            Sql::Text(v) => Value::String(v),
            _ => {
                return Err(rusqlite::Error::InvalidColumnType(
                    index,
                    name.into(),
                    row.get_ref(index)?.data_type(),
                ));
            }
        };
        value.insert(name.into(), field);
    }
    Ok(Value::Object(value))
}
fn rows<P: rusqlite::Params, T>(
    conn: &Connection,
    sql: &str,
    p: P,
    decode: fn(&Value) -> Result<T, StorageError>,
) -> Result<Vec<T>, StorageError> {
    let mut statement = db(conn.prepare_cached(sql))?;
    db(db(statement.query_map(p, document))?.collect::<rusqlite::Result<Vec<_>>>())?
        .iter()
        .map(decode)
        .collect()
}
fn limit(n: u16) -> Result<i64, StorageError> {
    if n == 0 || n > MAX_PAGE {
        Err(invalid("decision page limit must be 1..=MAX_PAGE"))
    } else {
        Ok(i64::from(n))
    }
}
fn bounded(s: &str, max: usize, empty: bool) -> Result<(), StorageError> {
    if s.len() > max || (!empty && s.trim().is_empty()) {
        Err(invalid("decision text is empty or exceeds its bound"))
    } else {
        Ok(())
    }
}
fn sha(s: &str) -> Result<(), StorageError> {
    if s.len() != 64
        || !s
            .bytes()
            .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
    {
        Err(invalid("decision digest must be lowercase SHA-256"))
    } else {
        Ok(())
    }
}
fn encoded(v: &Value, max: usize, object: bool) -> Result<String, StorageError> {
    if object && !v.is_object() {
        return Err(invalid("decision JSON must be an object"));
    }
    let value =
        serde_json::to_string(v).map_err(|_| invalid("decision JSON cannot be serialized"))?;
    if value.len() > max {
        return Err(invalid("decision JSON exceeds size bound"));
    }
    Ok(value)
}
fn revision(
    kind: &'static str,
    id: Id,
    expected: i64,
    current: i64,
) -> Result<StorageError, StorageError> {
    Ok(StorageError::conflict(
        RevisionDimension::Review,
        kind,
        id,
        Revision::new(expected).map_err(|_| invalid("expected revision is positive"))?,
        Revision::new(current).map_err(|_| corrupt("stored revision is positive"))?,
    ))
}
fn scope(v: &Value) -> Result<ConfigScope, StorageError> {
    ConfigScope::from_key(&text(v, "scope_key")?).ok_or_else(|| corrupt("invalid decision scope"))
}
fn actor(conn: &Connection, who: &ActorId, kind: PrincipalKind) -> Result<(), StorageError> {
    let stored: Option<String> = db(conn
        .query_row(
            "SELECT kind FROM actors WHERE id=?1",
            [who.0.as_str()],
            |row| row.get(0),
        )
        .optional())?;
    if stored.as_deref() != Some(principal_name(kind)) {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}
fn approval(v: &Value) -> Result<StoredApproval, StorageError> {
    Ok(StoredApproval {
        id: field_id(v, "id")?,
        scope: scope(v)?,
        revision: num(v, "revision")?,
        state: text(v, "state")?,
        operation_kind: text(v, "operation_kind")?,
        target_id: field_id(v, "target_id")?,
        target_revision: num(v, "target_revision")?,
        content_digest: text(v, "content_digest")?,
        scope_digest: text(v, "scope_digest")?,
        summary: text(v, "summary")?,
        artifact: optional_json(v, "artifact_json")?,
        payload: json(v, "payload_json")?,
        author: ActorId(field_id(v, "author_id")?),
        author_kind: parse_principal(&text(v, "author_kind")?)?,
        created_at_ms: num(v, "created_at")?,
        decided_by: optional_id(v, "decided_by")?.map(ActorId),
        decided_at_ms: optional_num(v, "decided_at")?,
        rationale: optional_text(v, "rationale")?,
        restore_epoch: num(v, "restore_epoch")?,
    })
}
fn audit(v: &Value) -> Result<StoredHumanDecision, StorageError> {
    Ok(StoredHumanDecision {
        source: ApprovalSource::parse(&text(v, "source")?)
            .ok_or_else(|| corrupt("unknown approval source"))?,
        proposal_id: field_id(v, "proposal_id")?,
        reviewed_proposal_revision: num(v, "reviewed_revision")?,
        decision: text(v, "decision")?,
        rationale: optional_text(v, "rationale")?,
        actor: ActorId(field_id(v, "actor_id")?),
        at_ms: num(v, "decided_at")?,
        content_digest: text(v, "content_digest")?,
        scope_digest: text(v, "scope_digest")?,
        reviewed_snapshot: json(v, "reviewed_snapshot_json")?,
    })
}
fn resume(v: &Value) -> Result<StoredApprovalResume, StorageError> {
    Ok(StoredApprovalResume {
        approval_id: field_id(v, "approval_id")?,
        revision: num(v, "revision")?,
        state: text(v, "state")?,
        principal: ActorId(field_id(v, "principal_id")?),
        principal_kind: parse_principal(&text(v, "principal_kind")?)?,
        invocation: json(v, "invocation_json")?,
        operation: text(v, "operation")?,
        schema_revision: num(v, "schema_revision")?,
        input: json(v, "input_json")?,
        request_key: field_id(v, "request_key")?,
        request_digest: text(v, "request_digest")?,
        grant_id: optional_id(v, "grant_id")?,
        created_at_ms: num(v, "created_at")?,
        updated_at_ms: num(v, "updated_at")?,
        result: optional_json(v, "result_json")?,
    })
}
fn notification(v: &Value) -> Result<StoredNotification, StorageError> {
    Ok(StoredNotification {
        id: field_id(v, "id")?,
        revision: num(v, "revision")?,
        user_id: ActorId(field_id(v, "user_id")?),
        project_id: optional_id(v, "project_id")?.map(ProjectId),
        root_id: optional_id(v, "root_id")?.map(RootId),
        decision_id: optional_id(v, "decision_id")?,
        category: text(v, "category")?,
        priority: num(v, "priority")?,
        title: text(v, "title")?,
        body: text(v, "body")?,
        source_id: text(v, "source_id")?,
        links: json(v, "links_json")?,
        requires_input: num(v, "requires_input")? != 0,
        read: num(v, "is_read")? != 0,
        acknowledged: num(v, "acknowledged")? != 0,
        created_at_ms: num(v, "created_at")?,
        updated_at_ms: num(v, "updated_at")?,
    })
}
fn one_approval(conn: &Connection, id: &Id) -> Result<Option<StoredApproval>, StorageError> {
    Ok(rows(
        conn,
        "SELECT * FROM decision_approvals WHERE id=?1",
        [id.as_str()],
        approval,
    )?
    .into_iter()
    .next())
}
fn one_resume(conn: &Connection, id: &Id) -> Result<Option<StoredApprovalResume>, StorageError> {
    Ok(rows(
        conn,
        "SELECT * FROM approval_resumes WHERE approval_id=?1",
        [id.as_str()],
        resume,
    )?
    .into_iter()
    .next())
}
fn one_notification(
    conn: &Connection,
    user: &ActorId,
    id: &Id,
) -> Result<Option<StoredNotification>, StorageError> {
    Ok(rows(
        conn,
        "SELECT * FROM notifications WHERE user_id=?1 AND id=?2",
        params![user.0.as_str(), id.as_str()],
        notification,
    )?
    .into_iter()
    .next())
}
const SOURCES: &str = "SELECT 'Settings' AS source,id,CASE WHEN scope_key='workspace' THEN NULL ELSE substr(scope_key,9) END AS project_id,created_at,state FROM configuration_proposals WHERE namespace='settings' UNION ALL SELECT 'FeaturePlan',p.id,w.project_id,p.requested_at,p.state FROM plan_proposals p JOIN work_items w ON w.id=p.feature_id UNION ALL SELECT 'Generic',id,project_id,created_at,state FROM decision_approvals";
fn source_ref(v: &Value) -> Result<ApprovalSourceRef, StorageError> {
    Ok(ApprovalSourceRef {
        source: ApprovalSource::parse(&text(v, "source")?)
            .ok_or_else(|| corrupt("unknown approval source"))?,
        id: field_id(v, "id")?,
        project_id: optional_id(v, "project_id")?.map(ProjectId),
        created_at_ms: num(v, "created_at")?,
    })
}
fn source_list(
    conn: &Connection,
    f: &ApprovalSourceFilter,
    after: Option<&ApprovalSourcePosition>,
    n: u16,
) -> Result<Vec<ApprovalSourceRef>, StorageError> {
    let n = limit(n)?;
    if f.state
        .as_deref()
        .is_some_and(|s| !PROPOSAL_STATES.contains(&s))
    {
        return Err(invalid("unknown approval state filter"));
    }
    let project = f.scope.project_id().map(|p| p.0.as_str());
    let at = after.map(|p| p.created_at_ms);
    let id = after.map(|p| p.id.as_str());
    let source = after.map(|p| p.source.as_str());
    rows(
        conn,
        &format!(
            "SELECT * FROM ({SOURCES}) WHERE (?1 IS NULL OR project_id=?1) AND (?2 IS NULL OR state=?2) AND (?3 IS NULL OR (created_at,id,source)<(?3,?4,?5)) ORDER BY created_at DESC,id DESC,source DESC LIMIT ?6"
        ),
        params![project, f.state, at, id, source, n],
        source_ref,
    )
}
fn notification_list(
    conn: &Connection,
    f: &NotificationFilter,
    after: Option<&NotificationPosition>,
    n: u16,
) -> Result<Vec<StoredNotification>, StorageError> {
    let n = limit(n)?;
    if f.categories.len() > 32 {
        return Err(invalid("too many notification categories"));
    }
    let mut sql = String::from("SELECT * FROM notifications WHERE user_id=?");
    let mut p = vec![Sql::Text(f.user_id.0.as_str().into())];
    if let Some(project) = f.scope.project_id() {
        sql.push_str(" AND project_id=?");
        p.push(Sql::Text(project.0.as_str().into()));
    }
    if let Some(read) = f.read {
        sql.push_str(" AND is_read=?");
        p.push(Sql::Integer(i64::from(read)));
    }
    if f.unacknowledged_only {
        sql.push_str(" AND acknowledged=0");
    }
    if f.completion_only {
        sql.push_str(" AND category='Completion'");
    }
    if !f.categories.is_empty() {
        sql.push_str(&format!(
            " AND category IN ({})",
            vec!["?"; f.categories.len()].join(",")
        ));
        for category in &f.categories {
            bounded(category, 128, false)?;
            p.push(Sql::Text(category.clone()));
        }
    }
    if let Some(after) = after {
        sql.push_str(" AND (created_at,id)<(?,?)");
        p.push(Sql::Integer(after.created_at_ms));
        p.push(Sql::Text(after.id.as_str().into()));
    }
    sql.push_str(" ORDER BY created_at DESC,id DESC LIMIT ?");
    p.push(Sql::Integer(n));
    rows(conn, &sql, params_from_iter(p), notification)
}
macro_rules! decision_reads {($t:ty)=>{impl DecisionRead for $t{
    fn notification_users(&self)->Result<Vec<ActorId>,StorageError>{
        let mut statement=db(self.conn.prepare_cached("SELECT a.id FROM notification_audience n JOIN actors a ON a.id=n.user_id WHERE n.singleton=1 AND a.kind='human'"))?;
        let values=db(db(statement.query_map([],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;
        values.into_iter().map(|v|id(v).map(ActorId)).collect()
    }
    fn list_approval_sources(&self,f:&ApprovalSourceFilter,a:Option<&ApprovalSourcePosition>,n:u16)->Result<Vec<ApprovalSourceRef>,StorageError>{source_list(self.conn,f,a,n)}
    fn approval(&self,id:&Id)->Result<Option<StoredApproval>,StorageError>{one_approval(self.conn,id)}
    fn human_decision(&self,s:ApprovalSource,id:&Id)->Result<Option<StoredHumanDecision>,StorageError>{Ok(rows(self.conn,"SELECT * FROM human_decisions WHERE source=?1 AND proposal_id=?2",params![s.as_str(),id.as_str()],audit)?.into_iter().next())}
    fn approval_resume(&self,id:&Id)->Result<Option<StoredApprovalResume>,StorageError>{one_resume(self.conn,id)}
    fn pending_approval_resumes(&self,after:Option<&Id>,n:u16)->Result<Vec<StoredApprovalResume>,StorageError>{rows(self.conn,"SELECT * FROM approval_resumes WHERE state IN ('Pending','Unknown') AND approval_id>?1 ORDER BY approval_id LIMIT ?2",params![after.map_or("",Id::as_str),limit(n)?],resume)}
    fn notification(&self,user:&ActorId,id:&Id)->Result<Option<StoredNotification>,StorageError>{one_notification(self.conn,user,id)}
    fn notification_by_source(&self,user:&ActorId,source:&str)->Result<Option<StoredNotification>,StorageError>{Ok(rows(self.conn,"SELECT * FROM notifications WHERE user_id=?1 AND source_id=?2",params![user.0.as_str(),source],notification)?.into_iter().next())}
    fn notifications(&self,f:&NotificationFilter,a:Option<&NotificationPosition>,n:u16)->Result<Vec<StoredNotification>,StorageError>{notification_list(self.conn,f,a,n)}
}};}
decision_reads!(Snapshot<'_>);
decision_reads!(Unit<'_>);

impl DecisionRepository for Unit<'_> {
    fn bind_notification_user(&mut self, user: &ActorId) -> Result<(), StorageError> {
        actor(self.conn, user, PrincipalKind::Human)?;
        db(self.conn.execute("INSERT INTO notification_audience(singleton,user_id) VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET user_id=excluded.user_id",[user.0.as_str()]))?;
        Ok(())
    }
    fn put_approval(
        &mut self,
        a: &StoredApproval,
        expected: Option<i64>,
    ) -> Result<(), StorageError> {
        if a.revision < 1
            || a.target_revision < 1
            || a.created_at_ms < 0
            || a.restore_epoch < 0
            || !PROPOSAL_STATES.contains(&a.state.as_str())
        {
            return Err(invalid("invalid approval counters or state"));
        }
        bounded(&a.operation_kind, 256, false)?;
        bounded(&a.summary, 65_536, false)?;
        sha(&a.content_digest)?;
        sha(&a.scope_digest)?;
        actor(self.conn, &a.author, a.author_kind)?;
        let payload = encoded(&a.payload, usize::MAX, true)?;
        let artifact = a
            .artifact
            .as_ref()
            .map(|v| encoded(v, MAX_DECISION_JSON_BYTES, true))
            .transpose()?;
        if let Some(rationale) = &a.rationale {
            bounded(rationale, 65_536, true)?;
        }
        if a.decided_by.is_some() != a.decided_at_ms.is_some()
            || a.decided_at_ms.is_some_and(|t| t < a.created_at_ms)
        {
            return Err(invalid("approval decision attribution is incomplete"));
        }
        if matches!(a.state.as_str(), "Approved" | "Rejected") {
            let human = a
                .decided_by
                .as_ref()
                .ok_or_else(|| invalid("human decision attribution is absent"))?;
            actor(self.conn, human, PrincipalKind::Human)?;
        }
        let current = one_approval(self.conn, &a.id)?;
        match (expected, current) {
            (None, None) => {
                if a.revision != 1
                    || a.state != "Pending"
                    || a.decided_by.is_some()
                    || a.rationale.is_some()
                {
                    return Err(invalid("a new approval is pending at revision 1"));
                }
                db(self.conn.execute("INSERT INTO decision_approvals(id,scope_key,project_id,revision,state,operation_kind,target_id,target_revision,content_digest,scope_digest,summary,artifact_json,payload_json,author_id,author_kind,created_at,decided_by,decided_at,rationale,restore_epoch) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,NULL,NULL,NULL,?17)",params![a.id.as_str(),a.scope.key(),a.scope.project_id().map(|p|p.0.as_str()),a.revision,a.state,a.operation_kind,a.target_id.as_str(),a.target_revision,a.content_digest,a.scope_digest,a.summary,artifact,payload,a.author.0.as_str(),principal_name(a.author_kind),a.created_at_ms,a.restore_epoch]))?;
            }
            (None, Some(_)) => {
                return Err(StorageError::Duplicate {
                    kind: "approval",
                    id: a.id.as_str().into(),
                });
            }
            (Some(_), None) => {
                return Err(StorageError::NotFound {
                    kind: "approval",
                    id: a.id.as_str().into(),
                });
            }
            (Some(e), Some(old)) => {
                if old.revision != e {
                    return Err(revision("approval", a.id.clone(), e, old.revision)?);
                }
                let mut content = a.clone();
                content.revision = old.revision;
                content.state = old.state.clone();
                content.decided_by = old.decided_by.clone();
                content.decided_at_ms = old.decided_at_ms;
                content.rationale = old.rationale.clone();
                if content != old
                    || old.state != "Pending"
                    || a.state == "Pending"
                    || a.revision
                        != e.checked_add(1)
                            .ok_or_else(|| invalid("approval revision overflow"))?
                {
                    return Err(invalid(
                        "approval content is immutable and only a pending decision advances once",
                    ));
                }
                db(self.conn.execute("UPDATE decision_approvals SET revision=?2,state=?3,decided_by=?4,decided_at=?5,rationale=?6 WHERE id=?1 AND revision=?7",params![a.id.as_str(),a.revision,a.state,a.decided_by.as_ref().map(|h|h.0.as_str()),a.decided_at_ms,a.rationale,e]))?;
            }
        }
        Ok(())
    }
    fn insert_human_decision(&mut self, a: &StoredHumanDecision) -> Result<(), StorageError> {
        if a.reviewed_proposal_revision < 1
            || a.at_ms < 0
            || !matches!(a.decision.as_str(), "Approved" | "Rejected")
        {
            return Err(invalid("invalid human decision revision or state"));
        }
        actor(self.conn, &a.actor, PrincipalKind::Human)?;
        sha(&a.content_digest)?;
        sha(&a.scope_digest)?;
        if let Some(r) = &a.rationale {
            bounded(r, 65_536, true)?;
        }
        let snapshot = encoded(&a.reviewed_snapshot, usize::MAX, true)?;
        if self.human_decision(a.source, &a.proposal_id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "human_decision",
                id: a.proposal_id.as_str().into(),
            });
        }
        let sql = match a.source {
            ApprovalSource::Settings => {
                "SELECT scope_key,revision,state,decided_by,decided_at FROM configuration_proposals WHERE namespace='settings' AND id=?1"
            }
            ApprovalSource::FeaturePlan => {
                "SELECT 'project:'||w.project_id AS scope_key,p.revision,p.state,p.decided_by,p.decided_at FROM plan_proposals p JOIN work_items w ON w.id=p.feature_id WHERE p.id=?1"
            }
            ApprovalSource::Generic => {
                "SELECT scope_key,revision,state,decided_by,decided_at FROM decision_approvals WHERE id=?1"
            }
        };
        type DecidedSource = (String, i64, String, Option<String>, Option<i64>);
        let source: Option<DecidedSource> = db(self
            .conn
            .query_row(sql, [a.proposal_id.as_str()], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional())?;
        let (scope, current, state, decided_by, decided_at) =
            source.ok_or_else(|| StorageError::NotFound {
                kind: "approval_source",
                id: a.proposal_id.as_str().into(),
            })?;
        if current
            != a.reviewed_proposal_revision
                .checked_add(1)
                .ok_or_else(|| invalid("reviewed revision overflow"))?
            || state != a.decision
            || decided_by.as_deref() != Some(a.actor.0.as_str())
            || decided_at != Some(a.at_ms)
        {
            return Err(invalid(
                "human audit must match the committed source decision and reviewed revision",
            ));
        }
        if a.source == ApprovalSource::Generic {
            let proposal = one_approval(self.conn, &a.proposal_id)?
                .ok_or_else(|| corrupt("decision source disappeared"))?;
            if proposal.content_digest != a.content_digest
                || proposal.scope_digest != a.scope_digest
            {
                return Err(invalid(
                    "human audit digests must match its immutable generic proposal",
                ));
            }
        }
        let scope = ConfigScope::from_key(&scope)
            .ok_or_else(|| corrupt("invalid approval source scope"))?;
        db(self.conn.execute("INSERT INTO human_decisions(source,proposal_id,project_id,reviewed_revision,decision,rationale,actor_id,decided_at,content_digest,scope_digest,reviewed_snapshot_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",params![a.source.as_str(),a.proposal_id.as_str(),scope.project_id().map(|p|p.0.as_str()),a.reviewed_proposal_revision,a.decision,a.rationale,a.actor.0.as_str(),a.at_ms,a.content_digest,a.scope_digest,snapshot]))?;
        Ok(())
    }
    fn put_approval_resume(
        &mut self,
        r: &StoredApprovalResume,
        expected: Option<i64>,
    ) -> Result<(), StorageError> {
        if r.revision < 1
            || r.schema_revision < 1
            || r.created_at_ms < 0
            || r.updated_at_ms < r.created_at_ms
            || !matches!(
                r.state.as_str(),
                "Pending" | "Admitted" | "Refused" | "Unknown"
            )
        {
            return Err(invalid("invalid resume state or counters"));
        }
        actor(self.conn, &r.principal, r.principal_kind)?;
        bounded(&r.operation, 256, false)?;
        sha(&r.request_digest)?;
        let invocation = encoded(&r.invocation, MAX_DECISION_JSON_BYTES, true)?;
        let input = encoded(&r.input, MAX_SAVED_OPERATION_BYTES, true)?;
        let result = r
            .result
            .as_ref()
            .map(|v| encoded(v, MAX_DECISION_JSON_BYTES, false))
            .transpose()?;
        let a = one_approval(self.conn, &r.approval_id)?.ok_or_else(|| StorageError::NotFound {
            kind: "approval",
            id: r.approval_id.as_str().into(),
        })?;
        if r.principal != a.author || r.principal_kind != a.author_kind {
            return Err(StorageError::ScopeDenied);
        }
        if a.state != "Approved" {
            return Err(invalid(
                "only an approved proposal can have a resume outbox",
            ));
        }
        if let Some(grant) = &r.grant_id {
            let grant = self
                .authority_grant(grant)?
                .ok_or_else(|| invalid("resume internal authority grant is absent"))?;
            if grant.actor != r.principal
                || !matches!(
                    grant.kind,
                    GrantKind::Operation | GrantKind::NativeExecution
                )
            {
                return Err(StorageError::ScopeDenied);
            }
        } else if r.principal_kind != PrincipalKind::Human {
            return Err(invalid(
                "non-human resume requires its internal request grant",
            ));
        }
        match (expected, one_resume(self.conn, &r.approval_id)?) {
            (None, None) => {
                if r.revision != 1 || r.state != "Pending" || r.result.is_some() {
                    return Err(invalid("new resume starts pending at revision 1"));
                }
                db(self.conn.execute("INSERT INTO approval_resumes(approval_id,revision,state,principal_id,principal_kind,invocation_json,operation,schema_revision,input_json,request_key,request_digest,grant_id,created_at,updated_at,result_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",params![r.approval_id.as_str(),r.revision,r.state,r.principal.0.as_str(),principal_name(r.principal_kind),invocation,r.operation,r.schema_revision,input,r.request_key.as_str(),r.request_digest,r.grant_id.as_ref().map(Id::as_str),r.created_at_ms,r.updated_at_ms,result]))?;
            }
            (None, Some(_)) => {
                return Err(StorageError::Duplicate {
                    kind: "approval_resume",
                    id: r.approval_id.as_str().into(),
                });
            }
            (Some(_), None) => {
                return Err(StorageError::NotFound {
                    kind: "approval_resume",
                    id: r.approval_id.as_str().into(),
                });
            }
            (Some(e), Some(old)) => {
                if old.revision != e {
                    return Err(revision(
                        "approval_resume",
                        r.approval_id.clone(),
                        e,
                        old.revision,
                    )?);
                }
                let mut content = r.clone();
                content.revision = old.revision;
                content.state = old.state.clone();
                content.updated_at_ms = old.updated_at_ms;
                content.result = old.result.clone();
                if content != old
                    || !matches!(old.state.as_str(), "Pending" | "Unknown")
                    || r.state == "Pending"
                    || r.revision
                        != e.checked_add(1)
                            .ok_or_else(|| invalid("resume revision overflow"))?
                    || r.updated_at_ms < old.updated_at_ms
                {
                    return Err(invalid(
                        "resume request binding is immutable and a terminal outcome is final",
                    ));
                }
                db(self.conn.execute("UPDATE approval_resumes SET revision=?2,state=?3,updated_at=?4,result_json=?5 WHERE approval_id=?1 AND revision=?6",params![r.approval_id.as_str(),r.revision,r.state,r.updated_at_ms,result,e]))?;
            }
        }
        Ok(())
    }
    fn put_notification(
        &mut self,
        n: &StoredNotification,
        expected: Option<i64>,
    ) -> Result<(), StorageError> {
        if n.revision < 1 || n.created_at_ms < 0 || n.updated_at_ms < n.created_at_ms {
            return Err(invalid("invalid notification counters"));
        }
        actor(self.conn, &n.user_id, PrincipalKind::Human)?;
        bounded(&n.title, 256, false)?;
        bounded(&n.body, 65_536, true)?;
        bounded(&n.category, 128, false)?;
        bounded(&n.source_id, 512, false)?;
        if !n.links.is_array() || n.links.as_array().is_some_and(|v| v.len() > 128) {
            return Err(invalid("notification links must be a bounded array"));
        }
        let links = encoded(&n.links, MAX_DECISION_JSON_BYTES, false)?;
        if let Some(root) = &n.root_id {
            let project: Option<String> = db(self
                .conn
                .query_row(
                    "SELECT project_id FROM work_items WHERE id=?1",
                    [root.0.as_str()],
                    |r| r.get(0),
                )
                .optional())?;
            if project.as_deref() != n.project_id.as_ref().map(|p| p.0.as_str()) {
                return Err(StorageError::ScopeDenied);
            }
        }
        match (expected, one_notification(self.conn, &n.user_id, &n.id)?) {
            (None, None) => {
                let exists:bool=db(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM notifications WHERE id=?1 OR (user_id=?2 AND source_id=?3))",params![n.id.as_str(),n.user_id.0.as_str(),n.source_id],|r|r.get(0)))?;
                if exists {
                    return Err(StorageError::Duplicate {
                        kind: "notification",
                        id: n.id.as_str().into(),
                    });
                }
                if n.revision != 1 || n.read || n.acknowledged {
                    return Err(invalid(
                        "a new notification is unread and unacknowledged at revision 1",
                    ));
                }
                db(self.conn.execute("INSERT INTO notifications(id,revision,user_id,project_id,root_id,decision_id,category,priority,title,body,source_id,links_json,requires_input,is_read,acknowledged,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",params![n.id.as_str(),n.revision,n.user_id.0.as_str(),n.project_id.as_ref().map(|p|p.0.as_str()),n.root_id.as_ref().map(|r|r.0.as_str()),n.decision_id.as_ref().map(Id::as_str),n.category,n.priority,n.title,n.body,n.source_id,links,n.requires_input,n.read,n.acknowledged,n.created_at_ms,n.updated_at_ms]))?;
            }
            (None, Some(_)) => {
                return Err(StorageError::Duplicate {
                    kind: "notification",
                    id: n.id.as_str().into(),
                });
            }
            (Some(_), None) => {
                return Err(StorageError::NotFound {
                    kind: "notification",
                    id: n.id.as_str().into(),
                });
            }
            (Some(e), Some(old)) => {
                if old.revision != e {
                    return Err(revision("notification", n.id.clone(), e, old.revision)?);
                }
                let mut content = n.clone();
                content.revision = old.revision;
                content.read = old.read;
                content.acknowledged = old.acknowledged;
                content.updated_at_ms = old.updated_at_ms;
                if content != old
                    || old.acknowledged && !n.acknowledged
                    || n.updated_at_ms < old.updated_at_ms
                    || n.revision
                        != e.checked_add(1)
                            .ok_or_else(|| invalid("notification revision overflow"))?
                {
                    return Err(invalid(
                        "notification ownership/content are immutable and acknowledgment is persistent",
                    ));
                }
                db(self.conn.execute("UPDATE notifications SET revision=?2,is_read=?3,acknowledged=?4,updated_at=?5 WHERE id=?1 AND user_id=?6 AND revision=?7",params![n.id.as_str(),n.revision,n.read,n.acknowledged,n.updated_at_ms,n.user_id.0.as_str(),e]))?;
            }
        }
        Ok(())
    }
}
