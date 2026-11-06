//! Typed GitHub source facts over the existing integration/intake/cursor tables. No HTTP,
//! inference, authority minting or duplicate external-effect ledger lives in the repository.
use super::{
    conn::db,
    repos::{Snapshot, Unit},
};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{Id, OperationId, OperationState, ProjectId, RootId};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};

fn invalid(m: &str) -> StorageError {
    StorageError::InvalidRecord(m.into())
}
fn encode(v: &impl Serialize) -> Result<String, StorageError> {
    serde_json::to_string(v).map_err(|_| invalid("invalid GitHub record"))
}
fn decode<T: DeserializeOwned>(s: String) -> Result<T, StorageError> {
    serde_json::from_str(&s).map_err(|_| invalid("invalid stored GitHub record"))
}
fn page(n: u16) -> Result<i64, StorageError> {
    if n == 0 || n > MAX_PAGE {
        Err(invalid("GitHub page must be1..500"))
    } else {
        Ok(i64::from(n))
    }
}
fn nonempty(s: &str) -> bool {
    !s.is_empty() && !s.contains('\0')
}
fn digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn hash(v: &impl Serialize) -> Result<String, StorageError> {
    let s = devworks_core::jcs::canonicalize(
        &serde_json::to_value(v).map_err(|_| invalid("GitHub canonical data"))?,
    )
    .map_err(|_| invalid("GitHub canonical data"))?;
    Ok(devworks_core::fingerprint::sha256_hex(s.as_bytes()))
}
fn key(k: &GithubIssueKey) -> Result<String, StorageError> {
    if k.number <= 0 {
        return Err(invalid("positive GitHub issue number required"));
    }
    Ok(format!("{}:{}", k.repository_id.as_str(), k.number))
}
fn source_id(k: &GithubIssueKey) -> Result<String, StorageError> {
    Ok(format!(
        "github-issue-{}",
        hash(&(k.repository_id.as_str(), k.number))?
    ))
}
fn conflict(id: &str, expected: i64, actual: i64) -> StorageError {
    StorageError::conflict(
        devworks_core::protocol::RevisionDimension::Scope,
        "github",
        Id::new(id).unwrap_or_else(|_| Id::new("github-record").expect("static id")),
        devworks_core::protocol::Revision::new(expected)
            .unwrap_or_else(|_| devworks_core::protocol::Revision::new(1).expect("positive")),
        devworks_core::protocol::Revision::new(actual)
            .unwrap_or_else(|_| devworks_core::protocol::Revision::new(1).expect("positive")),
    )
}
fn cas(row: i64, expected: Option<i64>, actual: Option<i64>, id: &str) -> Result<(), StorageError> {
    if actual != expected {
        return Err(conflict(id, expected.unwrap_or(1), actual.unwrap_or(1)));
    }
    if row
        != actual
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| invalid("GitHub revision overflow"))?
    {
        return Err(invalid("GitHub revision must advance once"));
    }
    Ok(())
}
fn atomic<T>(
    c: &Connection,
    f: impl FnOnce() -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    db(c.execute_batch("SAVEPOINT github_mutation"))?;
    match f() {
        Ok(v) => {
            db(c.execute_batch("RELEASE github_mutation"))?;
            Ok(v)
        }
        Err(e) => {
            let _ = c.execute_batch("ROLLBACK TO github_mutation; RELEASE github_mutation");
            Err(e)
        }
    }
}
fn record<T: DeserializeOwned>(
    c: &Connection,
    kind: &str,
    id: &str,
) -> Result<Option<T>, StorageError> {
    let v: Option<String> = db(c
        .query_row(
            "SELECT body_json FROM integration_records WHERE kind=?1 AND id=?2",
            params![kind, id],
            |r| r.get(0),
        )
        .optional())?;
    v.map(decode).transpose()
}
fn by_external<T: DeserializeOwned>(
    c: &Connection,
    kind: &str,
    key: &str,
) -> Result<Option<T>, StorageError> {
    let v: Option<String> = db(c
        .query_row(
            "SELECT body_json FROM integration_records WHERE kind=?1 AND external_key=?2",
            params![kind, key],
            |r| r.get(0),
        )
        .optional())?;
    v.map(decode).transpose()
}
#[allow(clippy::too_many_arguments)]
fn insert(
    c: &Connection,
    id: &str,
    kind: &str,
    external: &str,
    repo: Option<&Id>,
    project: Option<&ProjectId>,
    work: Option<&RootId>,
    revision: i64,
    state: &str,
    body: &impl Serialize,
) -> Result<(), StorageError> {
    db(c.execute("INSERT INTO integration_records(id,project_id,repository_id,root_id,kind,external_key,revision,state,body_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![id,project.map(|p|p.0.as_str()),repo.map(Id::as_str),work.map(|w|w.0.as_str()),kind,external,revision,state,encode(body)?]))?;
    Ok(())
}
fn update(
    c: &Connection,
    id: &str,
    kind: &str,
    revision: i64,
    state: &str,
    body: &impl Serialize,
) -> Result<(), StorageError> {
    db(c.execute(
        "UPDATE integration_records SET revision=?3,state=?4,body_json=?5 WHERE id=?1 AND kind=?2",
        params![id, kind, revision, state, encode(body)?],
    ))?;
    Ok(())
}
fn require_repository(c: &Connection, id: &Id) -> Result<(), StorageError> {
    let exists: bool = db(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM repositories WHERE id=?1 AND lifecycle!='Deleted')",
        [id.as_str()],
        |r| r.get(0),
    ))?;
    if exists {
        Ok(())
    } else {
        Err(invalid("GitHub registered repository missing/deleted"))
    }
}
fn scope(s: &GithubScope) -> Result<String, StorageError> {
    if s.repositories.len() > usize::from(MAX_PAGE) {
        return Err(invalid("GitHub repository scope batch exceeds500"));
    }
    encode(&s.repositories)
}
const RECORD_SCOPE: &str = "r.repository_id IN(SELECT value FROM json_each(?1)) AND (?2 IS NULL OR EXISTS(SELECT 1 FROM project_repositories m JOIN projects p ON p.id=m.project_id WHERE m.repository_id=r.repository_id AND m.project_id=?2 AND p.lifecycle!='Deleted'))";
const INTAKE_SCOPE: &str = "i.repository_id IN(SELECT value FROM json_each(?1)) AND (?2 IS NULL OR EXISTS(SELECT 1 FROM project_repositories m JOIN projects p ON p.id=m.project_id WHERE m.repository_id=i.repository_id AND m.project_id=?2 AND p.lifecycle!='Deleted')) AND (?2 IS NULL OR i.project_id IS NULL OR i.project_id=?2)";
fn account(c: &Connection) -> Result<Option<StoredGithubAccount>, StorageError> {
    by_external(c, "GithubAccount", "github.com")
}
fn repository(c: &Connection, id: &Id) -> Result<Option<StoredGithubRepository>, StorageError> {
    let v:Option<String>=db(c.query_row("SELECT body_json FROM integration_records WHERE kind='GithubRepository' AND repository_id=?1",[id.as_str()],|r|r.get(0)).optional())?;
    v.map(decode).transpose()
}

fn observation(c: &Connection, id: &Id) -> Result<Option<StoredGithubObservation>, StorageError> {
    record(c, "GithubObservation", id.as_str())
}
fn issue(
    c: &Connection,
    k: &GithubIssueKey,
) -> Result<Option<StoredGithubIssueIntake>, StorageError> {
    key(k)?;
    let v:Option<String>=db(c.query_row("SELECT remote_json FROM github_issue_intake WHERE repository_id=?1 AND issue_number=?2",params![k.repository_id.as_str(),k.number],|r|r.get(0)).optional())?;
    v.map(decode).transpose()
}
fn clock(c: &Connection) -> Result<GithubIntakeClock, StorageError> {
    Ok(db(c.query_row("SELECT revision,interval_seconds,last_admitted_at,next_admission_at FROM github_intake_clock WHERE singleton=1",[],|r|Ok(GithubIntakeClock{revision:r.get(0)?,interval_seconds:r.get(1)?,last_admitted_at_ms:r.get(2)?,next_admission_at_ms:r.get(3)?})).optional())?.unwrap_or(GithubIntakeClock{revision:1,interval_seconds:1800,last_admitted_at_ms:None,next_admission_at_ms:0}))
}
fn save_clock(c: &Connection, r: &GithubIntakeClock) -> Result<(), StorageError> {
    db(c.execute("INSERT INTO github_intake_clock(singleton,revision,interval_seconds,last_admitted_at,next_admission_at) VALUES(1,?1,?2,?3,?4) ON CONFLICT(singleton) DO UPDATE SET revision=excluded.revision,interval_seconds=excluded.interval_seconds,last_admitted_at=excluded.last_admitted_at,next_admission_at=excluded.next_admission_at",params![r.revision,r.interval_seconds,r.last_admitted_at_ms,r.next_admission_at_ms]))?;
    Ok(())
}
fn poll_key(repo: &Id, endpoint: &str) -> Result<String, StorageError> {
    if !nonempty(endpoint) {
        return Err(invalid("GitHub endpoint identity missing"));
    }
    Ok(format!("github:{}:{}", repo.as_str(), hash(&endpoint)?))
}
fn poll(
    c: &Connection,
    repo: &Id,
    endpoint: &str,
) -> Result<Option<StoredGithubPollCursor>, StorageError> {
    let v: Option<String> = db(c
        .query_row(
            "SELECT cursor_json FROM external_cursors WHERE source_id=?1 AND consumer='GithubPoll'",
            [poll_key(repo, endpoint)?],
            |r| r.get(0),
        )
        .optional())?;
    v.map(decode).transpose()
}
fn obligation_revision(c: &Connection, k: &GithubIssueKey) -> Result<i64, StorageError> {
    let rev: Option<i64> = db(c
        .query_row(
            "SELECT revision FROM integration_records WHERE id=?1 AND kind='GithubIssue'",
            [source_id(k)?],
            |r| r.get(0),
        )
        .optional())?;
    rev.ok_or_else(|| invalid("GitHub issue source missing"))
}
fn work(c: &Connection, id: &RootId) -> Result<(String, i64), StorageError> {
    db(c.query_row(
        "SELECT project_id,revision FROM work_items WHERE id=?1",
        [id.0.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ))
}
fn member(c: &Connection, project: &str, repo: &Id) -> Result<(), StorageError> {
    let ok:bool=db(c.query_row("SELECT EXISTS(SELECT 1 FROM project_repositories m JOIN projects p ON p.id=m.project_id WHERE m.project_id=?1 AND m.repository_id=?2 AND p.lifecycle!='Deleted')",params![project,repo.as_str()],|r|r.get(0)))?;
    if ok {
        Ok(())
    } else {
        Err(invalid(
            "GitHub work project does not own registered repository context",
        ))
    }
}
fn resolution(c: &Connection, id: &Id) -> Result<Option<StoredGithubResolution>, StorageError> {
    record(c, "GithubResolution", id.as_str())
}
fn observation_key(r: &StoredGithubObservation) -> Result<String, StorageError> {
    hash(&(
        &r.repository_id,
        &r.kind,
        &r.subject_id,
        &r.remote_revision,
        &r.url,
        &r.head,
        &r.payload,
        r.source_updated_at_ms,
    ))
}
fn validate_observation(r: &StoredGithubObservation) -> Result<(), StorageError> {
    if !matches!(
        r.kind.as_str(),
        "Issue"
            | "IssueComment"
            | "Push"
            | "PullRequest"
            | "Review"
            | "ReviewComment"
            | "CheckRun"
            | "CheckSuite"
            | "WorkflowRun"
    ) || !nonempty(&r.subject_id)
        || !nonempty(&r.remote_revision)
        || !nonempty(&r.url)
        || !r.payload.is_object()
        || r.observed_at_ms < 0
        || r.source_updated_at_ms.is_some_and(|n| n < 0)
    {
        return Err(invalid("invalid GitHub observation"));
    }
    Ok(())
}
fn validate_issue(c: &Connection, r: &StoredGithubIssueIntake) -> Result<(), StorageError> {
    key(&r.key)?;
    require_repository(c, &r.key.repository_id)?;
    if !matches!(
        r.state.as_str(),
        "Queued" | "Triaging" | "AwaitingDecision" | "Working" | "Resolved" | "Closed"
    ) || r.project_id.is_some() != r.work_id.is_some()
    {
        return Err(invalid("invalid GitHub intake state/routing"));
    }
    let source = observation(c, &r.observation_id)?
        .ok_or_else(|| invalid("GitHub issue observation missing"))?;
    if source.repository_id != r.key.repository_id
        || source.kind != "Issue"
        || source.subject_id != r.key.number.to_string()
        || source.id.as_str() != r.remote_revision
    {
        return Err(invalid("GitHub intake differs from exact observed issue"));
    }
    if let (Some(project), Some(id)) = (&r.project_id, &r.work_id) {
        let (p, _) = work(c, id)?;
        if p != project.0.as_str() {
            return Err(invalid("GitHub intake work/project mismatch"));
        }
        member(c, &p, &r.key.repository_id)?;
    }
    if let Some(op) = &r.operation_id {
        let exists: bool = db(c.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1)",
            [op.0.as_str()],
            |r| r.get(0),
        ))?;
        if !exists {
            return Err(invalid("GitHub intake operation missing"));
        }
    }
    Ok(())
}
fn save_issue(c: &Connection, r: &StoredGithubIssueIntake) -> Result<(), StorageError> {
    db(c.execute("INSERT INTO github_issue_intake(repository_id,issue_number,revision,state,project_id,work_id,remote_json,operation_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(repository_id,issue_number) DO UPDATE SET revision=excluded.revision,state=excluded.state,project_id=excluded.project_id,work_id=excluded.work_id,remote_json=excluded.remote_json,operation_id=excluded.operation_id",params![r.key.repository_id.as_str(),r.key.number,r.revision,r.state,r.project_id.as_ref().map(|p|p.0.as_str()),r.work_id.as_ref().map(|w|w.0.as_str()),encode(r)?,r.operation_id.as_ref().map(|o|o.0.as_str())]))?;
    Ok(())
}
macro_rules! reads{($t:ty)=>{impl GithubRead for $t{
 fn github_discovered_repository(&self,id:&Id)->Result<Option<StoredGithubRepository>,StorageError>{record(self.conn,"GithubRepository",id.as_str())}
 fn github_account_repositories(&self,account:&Id,owner:Option<&str>,after:Option<&Id>,limit:u16)->Result<Vec<StoredGithubRepository>,StorageError>{let n=page(limit)?;let mut q=db(self.conn.prepare_cached("SELECT body_json FROM integration_records WHERE kind='GithubRepository' AND json_extract(body_json,'$.account_id')=?1 AND (?2 IS NULL OR json_extract(body_json,'$.owner')=?2 COLLATE NOCASE) AND (?3 IS NULL OR id>?3) ORDER BY id LIMIT ?4"))?;let rows=db(db(q.query_map(params![account.as_str(),owner,after.map(Id::as_str),n],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(decode).collect()}

 fn github_account(&self)->Result<Option<StoredGithubAccount>,StorageError>{account(self.conn)}
 fn github_repository(&self,id:&Id)->Result<Option<StoredGithubRepository>,StorageError>{repository(self.conn,id)}
 fn github_repositories(&self,s:&GithubScope,after:Option<&Id>,limit:u16)->Result<Vec<StoredGithubRepository>,StorageError>{let n=page(limit)?;let mut q=db(self.conn.prepare_cached(&format!("SELECT r.body_json FROM integration_records r WHERE r.kind='GithubRepository' AND {RECORD_SCOPE} AND (?3 IS NULL OR r.id>?3) ORDER BY r.id LIMIT ?4")))?;let rows=db(db(q.query_map(params![scope(s)?,s.project_id.as_ref().map(|p|p.0.as_str()),after.map(Id::as_str),n],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(decode).collect()}
 fn github_observation(&self,id:&Id,s:&GithubScope)->Result<Option<StoredGithubObservation>,StorageError>{let v:Option<String>=db(self.conn.query_row(&format!("SELECT r.body_json FROM integration_records r WHERE r.id=?3 AND r.kind='GithubObservation' AND {RECORD_SCOPE}"),params![scope(s)?,s.project_id.as_ref().map(|p|p.0.as_str()),id.as_str()],|r|r.get(0)).optional())?;v.map(decode).transpose()}
 fn github_observations(&self,f:&GithubObservationFilter,after:Option<&Id>,limit:u16)->Result<Vec<StoredGithubObservation>,StorageError>{let n=page(limit)?;let mut q=db(self.conn.prepare_cached(&format!("SELECT r.body_json FROM integration_records r WHERE r.kind='GithubObservation' AND {RECORD_SCOPE} AND (?3 IS NULL OR r.id>?3) AND (json_array_length(?4)=0 OR json_extract(r.body_json,'$.kind') IN(SELECT value FROM json_each(?4))) AND (?5 IS NULL OR json_extract(r.body_json,'$.subject_id')=?5) AND (?6 IS NULL OR json_extract(r.body_json,'$.head')=?6) ORDER BY r.id LIMIT ?7")))?;let rows=db(db(q.query_map(params![scope(&f.scope)?,f.scope.project_id.as_ref().map(|p|p.0.as_str()),after.map(Id::as_str),encode(&f.kinds)?,f.subject_id,f.head,n],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(decode).collect()}
 fn github_poll_cursor(&self,r:&Id,e:&str)->Result<Option<StoredGithubPollCursor>,StorageError>{poll(self.conn,r,e)}
 fn github_issue(&self,k:&GithubIssueKey,s:&GithubScope)->Result<Option<StoredGithubIssueIntake>,StorageError>{key(k)?;let v:Option<String>=db(self.conn.query_row(&format!("SELECT i.remote_json FROM github_issue_intake i WHERE i.repository_id=?3 AND i.issue_number=?4 AND {INTAKE_SCOPE}"),params![scope(s)?,s.project_id.as_ref().map(|p|p.0.as_str()),k.repository_id.as_str(),k.number],|r|r.get(0)).optional())?;v.map(decode).transpose()}
 fn github_issues(&self,f:&GithubIntakeFilter,after:Option<&GithubIssueKey>,limit:u16)->Result<Vec<StoredGithubIssueIntake>,StorageError>{let n=page(limit)?;if let Some(k)=after{key(k)?;}let mut q=db(self.conn.prepare_cached(&format!("SELECT i.remote_json FROM github_issue_intake i WHERE {INTAKE_SCOPE} AND (?3 IS NULL OR (i.repository_id,i.issue_number)>(?3,?4)) AND (json_array_length(?5)=0 OR i.state IN(SELECT value FROM json_each(?5))) ORDER BY i.repository_id,i.issue_number LIMIT ?6")))?;let rows=db(db(q.query_map(params![scope(&f.scope)?,f.scope.project_id.as_ref().map(|p|p.0.as_str()),after.map(|k|k.repository_id.as_str()),after.map(|k|k.number),encode(&f.states)?,n],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(decode).collect()}
 fn github_intake_clock(&self)->Result<GithubIntakeClock,StorageError>{clock(self.conn)}
 fn github_obligation_revision(&self,k:&GithubIssueKey)->Result<i64,StorageError>{obligation_revision(self.conn,k)}
 fn github_work_links(&self,k:&GithubIssueKey,project:Option<&ProjectId>,after:Option<&RootId>,limit:u16)->Result<Vec<GithubWorkLink>,StorageError>{let n=page(limit)?;let mut q=db(self.conn.prepare_cached("SELECT w.id,w.project_id FROM integration_links l JOIN work_items w ON w.id=l.work_id WHERE l.record_id=?1 AND l.purpose='GithubIssue' AND (?2 IS NULL OR w.project_id=?2) AND (?3 IS NULL OR w.id>?3) ORDER BY w.id LIMIT ?4"))?;let rows=db(db(q.query_map(params![source_id(k)?,project.map(|p|p.0.as_str()),after.map(|w|w.0.as_str()),n],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(|(w,p)|Ok(GithubWorkLink{work_id:RootId(Id::new(w).map_err(|_|invalid("work identity"))?),project_id:ProjectId(Id::new(p).map_err(|_|invalid("project identity"))?)})).collect()}
 fn github_resolution(&self,id:&Id)->Result<Option<StoredGithubResolution>,StorageError>{resolution(self.conn,id)}
 fn github_resolutions(&self,k:&GithubIssueKey,after:Option<&Id>,limit:u16)->Result<Vec<StoredGithubResolution>,StorageError>{let n=page(limit)?;let mut q=db(self.conn.prepare_cached("SELECT body_json FROM integration_records WHERE kind='GithubResolution' AND repository_id=?1 AND json_extract(body_json,'$.key.number')=?2 AND (?3 IS NULL OR id>?3) ORDER BY id LIMIT ?4"))?;let rows=db(db(q.query_map(params![k.repository_id.as_str(),k.number,after.map(Id::as_str),n],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(decode).collect()}
 fn github_resolution_witnesses(&self,id:&Id,project:Option<&ProjectId>,after:Option<&RootId>,limit:u16)->Result<Vec<StoredGithubResolutionWitness>,StorageError>{let n=page(limit)?;let mut q=db(self.conn.prepare_cached("SELECT body_json FROM integration_records WHERE kind='GithubResolutionWitness' AND json_extract(body_json,'$.resolution_id')=?1 AND (?2 IS NULL OR project_id=?2) AND (?3 IS NULL OR root_id>?3) ORDER BY root_id LIMIT ?4"))?;let rows=db(db(q.query_map(params![id.as_str(),project.map(|p|p.0.as_str()),after.map(|w|w.0.as_str()),n],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(decode).collect()}
}};}
reads!(Snapshot<'_>);
reads!(Unit<'_>);
impl GithubRepository for Unit<'_> {
    fn put_github_account(
        &mut self,
        r: &StoredGithubAccount,
        expected: Option<i64>,
    ) -> Result<(), StorageError> {
        let old = account(self.conn)?;
        cas(
            r.revision,
            expected,
            old.as_ref().map(|r| r.revision),
            r.id.as_str(),
        )?;
        if r.credential_revision < 1
            || r.permission_revision < 1
            || r.observed_at_ms < 0
            || r.remote_user_id.is_some() != r.login.is_some()
            || !matches!(
                r.state.as_str(),
                "Disconnected"
                    | "Pending"
                    | "Connected"
                    | "Unavailable"
                    | "ReauthorizationRequired"
            )
            || old.as_ref().is_some_and(|o| {
                o.id != r.id
                    || o.permission_revision > r.permission_revision
                    || o.credential_revision > r.credential_revision
            })
        {
            return Err(invalid("invalid GitHub account identity/revision"));
        }
        if old.is_some() {
            update(
                self.conn,
                r.id.as_str(),
                "GithubAccount",
                r.revision,
                &r.state,
                r,
            )
        } else {
            insert(
                self.conn,
                r.id.as_str(),
                "GithubAccount",
                "github.com",
                None,
                None,
                None,
                r.revision,
                &r.state,
                r,
            )
        }
    }
    fn put_github_repository(
        &mut self,
        r: &StoredGithubRepository,
        expected: Option<i64>,
    ) -> Result<(), StorageError> {
        if let Some(id) = &r.repository_id {
            require_repository(self.conn, id)?;
        }
        let a = account(self.conn)?.ok_or_else(|| invalid("GitHub account missing"))?;
        let old: Option<StoredGithubRepository> =
            record(self.conn, "GithubRepository", r.id.as_str())?;
        cas(
            r.revision,
            expected,
            old.as_ref().map(|r| r.revision),
            r.id.as_str(),
        )?;
        if r.account_id != a.id
            || r.permission_revision < 1
            || !nonempty(&r.remote_repository_id)
            || !nonempty(&r.owner)
            || !nonempty(&r.name)
            || !nonempty(&r.url)
            || !r.permissions.is_object()
            || r.observed_at_ms < 0
            || old.as_ref().is_some_and(|o| {
                o.remote_repository_id != r.remote_repository_id
                    || o.permission_revision > r.permission_revision
            })
        {
            return Err(invalid("GitHub repository binding mismatch"));
        }
        if let Some(reg) = &r.repository_id {
            let other:bool=db(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM integration_records WHERE kind='GithubRepository' AND repository_id=?1 AND id!=?2)",params![reg.as_str(),r.id.as_str()],|r|r.get(0)))?;
            if other {
                return Err(invalid(
                    "registered repository already has a GitHub identity",
                ));
            }
        }
        if old.is_some() {
            db(self.conn.execute("UPDATE integration_records SET repository_id=?2,revision=?3,body_json=?4 WHERE id=?1 AND kind='GithubRepository'",params![r.id.as_str(),r.repository_id.as_ref().map(Id::as_str),r.revision,encode(r)?]))?;
            Ok(())
        } else {
            insert(
                self.conn,
                r.id.as_str(),
                "GithubRepository",
                &r.remote_repository_id,
                r.repository_id.as_ref(),
                None,
                None,
                r.revision,
                "Observed",
                r,
            )
        }
    }
    fn append_github_observation(
        &mut self,
        r: &StoredGithubObservation,
    ) -> Result<StoredGithubObservation, StorageError> {
        validate_observation(r)?;
        require_repository(self.conn, &r.repository_id)?;
        let external = observation_key(r)?;
        if let Some(old) = by_external(self.conn, "GithubObservation", &external)? {
            return Ok(old);
        }
        if observation(self.conn, &r.id)?.is_some() {
            return Err(invalid("immutable GitHub observation ID reused"));
        }
        insert(
            self.conn,
            r.id.as_str(),
            "GithubObservation",
            &external,
            Some(&r.repository_id),
            None,
            None,
            1,
            "Observed",
            r,
        )?;
        Ok(r.clone())
    }
    fn put_github_poll_cursor(
        &mut self,
        r: &StoredGithubPollCursor,
        expected: Option<i64>,
    ) -> Result<(), StorageError> {
        require_repository(self.conn, &r.repository_id)?;
        let old = poll(self.conn, &r.repository_id, &r.endpoint)?;
        cas(
            r.revision,
            expected,
            old.map(|r| r.revision),
            r.repository_id.as_str(),
        )?;
        if r.next_poll_at_ms < 0
            || r.last_polled_at_ms.is_some_and(|n| n < 0)
            || r.failure_count < 0
        {
            return Err(invalid("invalid GitHub poll timing"));
        }
        db(self.conn.execute("INSERT INTO external_cursors(source_id,consumer,revision,cursor_json) VALUES(?1,'GithubPoll',?2,?3) ON CONFLICT(source_id,consumer) DO UPDATE SET revision=excluded.revision,cursor_json=excluded.cursor_json",params![poll_key(&r.repository_id,&r.endpoint)?,r.revision,encode(r)?]))?;
        Ok(())
    }
    fn put_github_issue(
        &mut self,
        r: &StoredGithubIssueIntake,
        expected: Option<i64>,
    ) -> Result<(), StorageError> {
        let old = issue(self.conn, &r.key)?;
        cas(
            r.revision,
            expected,
            old.as_ref().map(|r| r.revision),
            r.key.repository_id.as_str(),
        )?;
        validate_issue(self.conn, r)?;
        if (old.is_none()
            && (r.state != "Queued"
                || r.operation_id.is_some()
                || r.project_id.is_some()
                || r.last_triaged_revision.is_some()))
            || (r.state == "Triaging"
                && old
                    .as_ref()
                    .is_none_or(|o| o.state != "Triaging" || o.operation_id != r.operation_id))
            || old
                .as_ref()
                .is_some_and(|o| o.operation_id != r.operation_id)
        {
            return Err(invalid(
                "triage claims must use the atomic global admission seam",
            ));
        }
        atomic(self.conn, || {
            if old.is_none() {
                insert(
                    self.conn,
                    &source_id(&r.key)?,
                    "GithubIssue",
                    &key(&r.key)?,
                    Some(&r.key.repository_id),
                    None,
                    None,
                    1,
                    "Source",
                    &r.key,
                )?;
            }
            save_issue(self.conn, r)
        })
    }
    fn set_github_intake_interval(
        &mut self,
        expected: i64,
        seconds: i64,
    ) -> Result<GithubIntakeClock, StorageError> {
        let mut c = clock(self.conn)?;
        if c.revision != expected {
            return Err(conflict("github-intake-clock", expected, c.revision));
        }
        if seconds <= 0 {
            return Err(invalid("GitHub interval must be positive"));
        }
        let ms = seconds
            .checked_mul(1000)
            .ok_or_else(|| invalid("GitHub interval overflow"))?;
        c.revision = c
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("GitHub clock revision overflow"))?;
        c.interval_seconds = seconds;
        c.next_admission_at_ms = c
            .last_admitted_at_ms
            .map(|last| {
                last.checked_add(ms)
                    .ok_or_else(|| invalid("GitHub clock overflow"))
            })
            .transpose()?
            .unwrap_or(0);
        save_clock(self.conn, &c)?;
        Ok(c)
    }
    fn admit_github_issue(
        &mut self,
        k: &GithubIssueKey,
        expected_issue: i64,
        expected_clock: i64,
        op: &OperationId,
    ) -> Result<Option<GithubAdmission>, StorageError> {
        let mut r = issue(self.conn, k)?.ok_or_else(|| invalid("GitHub intake missing"))?;
        let mut c = clock(self.conn)?;
        let operation = self
            .operation(op)?
            .ok_or_else(|| invalid("GitHub triage operation missing"))?;
        if operation.capability != "github.triage"
            || operation.payload["repositoryId"].as_str() != Some(k.repository_id.as_str())
            || operation.payload["issueNumber"].as_i64() != Some(k.number)
        {
            return Err(invalid("GitHub triage operation source mismatch"));
        }
        if r.operation_id.as_ref() == Some(op) {
            return Ok(Some(GithubAdmission {
                issue: r,
                clock: c,
                operation,
            }));
        }
        if let Some(previous) = &r.operation_id {
            if self
                .operation(previous)?
                .is_none_or(|o| !is_terminal(o.state))
            {
                return Err(StorageError::Busy);
            }
        }
        if operation.state != OperationState::IntentRecorded
            || operation.payload["sourceRevision"].as_str() != Some(r.remote_revision.as_str())
            || r.state != "Queued"
        {
            return Err(invalid(
                "GitHub triage requires a queued exact-source real intent",
            ));
        }
        if r.revision != expected_issue {
            return Err(conflict(&source_id(k)?, expected_issue, r.revision));
        }
        if c.revision != expected_clock {
            return Err(conflict("github-intake-clock", expected_clock, c.revision));
        }
        if self.now < c.next_admission_at_ms || c.last_admitted_at_ms.is_some_and(|n| self.now < n)
        {
            return Ok(None);
        }
        let used: bool = db(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM github_issue_intake WHERE operation_id=?1)",
            [op.0.as_str()],
            |r| r.get(0),
        ))?;
        if used {
            return Err(invalid(
                "GitHub triage operation already owns another source",
            ));
        }
        validate_issue(self.conn, &r)?;
        c.revision = c
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("GitHub clock revision overflow"))?;
        c.last_admitted_at_ms = Some(self.now);
        c.next_admission_at_ms = self
            .now
            .checked_add(
                c.interval_seconds
                    .checked_mul(1000)
                    .ok_or_else(|| invalid("GitHub interval overflow"))?,
            )
            .ok_or_else(|| invalid("GitHub clock overflow"))?;
        r.revision = r
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("GitHub intake revision overflow"))?;
        r.state = "Triaging".into();
        r.operation_id = Some(op.clone());
        atomic(self.conn, || {
            save_clock(self.conn, &c)?;
            save_issue(self.conn, &r)?;
            Ok(Some(GithubAdmission {
                issue: r,
                clock: c,
                operation,
            }))
        })
    }
    fn link_github_work(
        &mut self,
        k: &GithubIssueKey,
        w: &RootId,
        expected: i64,
    ) -> Result<(), StorageError> {
        issue(self.conn, k)?.ok_or_else(|| invalid("GitHub source missing"))?;
        let (p, revision) = work(self.conn, w)?;
        if revision != expected {
            return Err(conflict(w.0.as_str(), expected, revision));
        }
        member(self.conn, &p, &k.repository_id)?;
        let sid = source_id(k)?;
        atomic(self.conn, || {
            let changed=db(self.conn.execute("INSERT INTO integration_links(record_id,work_id,purpose) VALUES(?1,?2,'GithubIssue') ON CONFLICT DO NOTHING",params![sid,w.0.as_str()]))?;
            if changed > 0 {
                let revision = obligation_revision(self.conn, k)?
                    .checked_add(1)
                    .ok_or_else(|| invalid("Github obligation revision overflow"))?;
                db(self.conn.execute(
                    "UPDATE integration_records SET revision=?2 WHERE id=?1",
                    params![sid, revision],
                ))?;
            }
            Ok(())
        })
    }
    fn insert_github_resolution(
        &mut self,
        r: &StoredGithubResolution,
    ) -> Result<StoredGithubResolution, StorageError> {
        key(&r.key)?;
        if r.state != "Draft"
            || !digest(&r.completion_digest)
            || !digest(&r.summary_digest)
            || r.source_revision.is_empty()
            || r.obligation_revision != obligation_revision(self.conn, &r.key)?
        {
            return Err(invalid("invalid GitHub completion source binding"));
        }
        let external = format!("{}:{}", key(&r.key)?, r.completion_digest);
        if let Some(old) = by_external(self.conn, "GithubResolution", &external)? {
            return Ok(old);
        }
        let current = issue(self.conn, &r.key)?.ok_or_else(|| invalid("GitHub source missing"))?;
        if current.remote_revision != r.source_revision {
            return Err(invalid("GitHub resolution source changed"));
        }
        for (op, cap) in [
            (&r.comment_operation_id, "github.resolution.comment"),
            (&r.close_operation_id, "github.resolution.close"),
        ] {
            let o = self
                .operation(op)?
                .ok_or_else(|| invalid("Github resolution operation missing"))?;
            if o.capability != cap
                || o.state != OperationState::IntentRecorded
                || o.payload["repositoryId"].as_str() != Some(r.key.repository_id.as_str())
                || o.payload["issueNumber"].as_i64() != Some(r.key.number)
                || o.payload["completionDigest"] != r.completion_digest
            {
                return Err(invalid("GitHub resolution operation binding mismatch"));
            }
        }
        insert(
            self.conn,
            r.id.as_str(),
            "GithubResolution",
            &external,
            Some(&r.key.repository_id),
            None,
            None,
            1,
            "Draft",
            r,
        )?;
        Ok(r.clone())
    }
    fn insert_github_resolution_witness(
        &mut self,
        r: &StoredGithubResolutionWitness,
    ) -> Result<(), StorageError> {
        let binding = resolution(self.conn, &r.resolution_id)?
            .ok_or_else(|| invalid("GitHub resolution missing"))?;
        let external = format!("{}:{}", r.resolution_id.as_str(), r.work_id.0.as_str());
        if let Some(old) = by_external::<StoredGithubResolutionWitness>(
            self.conn,
            "GithubResolutionWitness",
            &external,
        )? {
            return if old == *r {
                Ok(())
            } else {
                Err(invalid("immutable GitHub resolution witness changed"))
            };
        }
        if binding.state != "Draft" || !r.evidence.is_object() {
            return Err(invalid("GitHub witness cannot mutate a sealed resolution"));
        }
        let (p, rev) = work(self.conn, &r.work_id)?;
        if p != r.project_id.0.as_str() || rev != r.work_revision {
            return Err(invalid("Github witness work/project/revision changed"));
        }
        member(self.conn, &p, &binding.key.repository_id)?;
        let linked:bool=db(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM integration_links WHERE record_id=?1 AND work_id=?2 AND purpose='GithubIssue')",params![source_id(&binding.key)?,r.work_id.0.as_str()],|r|r.get(0)))?;
        if !linked {
            return Err(invalid(
                "GitHub witness work not included in remote obligation set",
            ));
        }
        insert(
            self.conn,
            &format!("github-witness-{}", hash(&external)?),
            "GithubResolutionWitness",
            &external,
            Some(&binding.key.repository_id),
            Some(&r.project_id),
            Some(&r.work_id),
            1,
            "Captured",
            r,
        )
    }
    fn seal_github_resolution(
        &mut self,
        id: &Id,
        count: i64,
    ) -> Result<StoredGithubResolution, StorageError> {
        let mut r =
            resolution(self.conn, id)?.ok_or_else(|| invalid("GitHub resolution missing"))?;
        if r.state == "Sealed" {
            return Ok(r);
        }
        if count <= 0
            || r.obligation_revision != obligation_revision(self.conn, &r.key)?
            || issue(self.conn, &r.key)?.is_none_or(|i| i.remote_revision != r.source_revision)
        {
            return Err(invalid("Github completion obligation/source changed"));
        }
        let(actual,stale):(i64,i64)=db(self.conn.query_row("SELECT count(*),coalesce(sum(CASE WHEN w.id IS NULL OR w.project_id!=r.project_id OR w.revision!=json_extract(r.body_json,'$.work_revision') THEN 1 ELSE 0 END),0) FROM integration_records r LEFT JOIN work_items w ON w.id=r.root_id WHERE r.kind='GithubResolutionWitness' AND json_extract(r.body_json,'$.resolution_id')=?1",[id.as_str()],|r|Ok((r.get(0)?,r.get(1)?))))?;
        let obligations: i64 = db(self.conn.query_row(
            "SELECT count(*) FROM integration_links WHERE record_id=?1 AND purpose='GithubIssue'",
            [source_id(&r.key)?],
            |r| r.get(0),
        ))?;
        if actual != count || actual != obligations || stale > 0 {
            return Err(invalid("GitHub completion witness set is incomplete/stale"));
        }
        r.state = "Sealed".into();
        update(self.conn, id.as_str(), "GithubResolution", 2, "Sealed", &r)?;
        Ok(r)
    }
}
