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
    devworks_core::fingerprint::is_sha256_hex(s)
}
fn hash(v: &impl Serialize) -> Result<String, StorageError> {
    devworks_core::fingerprint::canonical_digest(
        &serde_json::to_value(v).map_err(|_| invalid("GitHub canonical data"))?,
    )
    .map_err(|_| invalid("GitHub canonical data"))
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
    match (
        Id::new(id),
        devworks_core::protocol::Revision::new(expected),
        devworks_core::protocol::Revision::new(actual),
    ) {
        (Ok(id), Ok(e), Ok(a)) => StorageError::conflict(
            devworks_core::protocol::RevisionDimension::Scope,
            "github",
            id,
            e,
            a,
        ),
        _ => invalid("invalid GitHub conflict identity/revision"),
    }
}

fn cas(row: i64, expected: Option<i64>, actual: Option<i64>, id: &str) -> Result<(), StorageError> {
    if actual != expected {
        return Err(match (expected, actual) {
            (None, Some(_)) => StorageError::Duplicate {
                kind: "github",
                id: id.into(),
            },
            (Some(_), None) => StorageError::NotFound {
                kind: "github",
                id: id.into(),
            },
            (Some(e), Some(a)) => conflict(id, e, a),
            _ => invalid("GitHub revision mismatch"),
        });
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
    if record::<StoredGithubRepository>(c, "GithubRepository", id.as_str())?.is_some() {
        Ok(())
    } else {
        Err(invalid("canonical observed GitHub repository missing"))
    }
}
fn binding(c: &Connection, id: &Id) -> Result<Option<StoredGithubRepositoryBinding>, StorageError> {
    by_external(c, "GithubRepositoryBinding", id.as_str())
}
fn binding_projection(
    c: &Connection,
    id: &Id,
) -> Result<Option<StoredGithubRepositoryBinding>, StorageError> {
    let Some(mut r) = binding(c, id)? else {
        return Ok(None);
    };
    if r.state == "Active" {
        let live:bool=db(c.query_row("SELECT EXISTS(SELECT 1 FROM repositories n LEFT JOIN repository_details d ON d.repository_id=n.id WHERE n.id=?1 AND coalesce(d.lifecycle,'Active')!='Deleted' AND d.remote_url=?2)",params![id.as_str(),r.registered_remote],|r|r.get(0)))?;
        if !live {
            r.state = "Stale".into();
        }
    }
    Ok(Some(r))
}

const BINDING_NATIVE: &str = "FROM integration_records binding JOIN repositories native ON native.id=binding.repository_id LEFT JOIN repository_details details ON details.repository_id=native.id";
fn canonical_scope(alias: &str) -> String {
    format!(
        "EXISTS(SELECT 1 {BINDING_NATIVE} WHERE binding.kind='GithubRepositoryBinding' AND binding.state='Active' AND coalesce(details.lifecycle,'Active')!='Deleted' AND details.remote_url=json_extract(binding.body_json,'$.registered_remote') AND json_extract(binding.body_json,'$.github_repository_id')={alias} AND binding.repository_id IN(SELECT value FROM json_each(?1)) AND (?2 IS NULL OR EXISTS(SELECT 1 FROM project_repositories m JOIN projects p ON p.id=m.project_id WHERE m.repository_id=binding.repository_id AND m.project_id=?2 AND p.lifecycle!='Deleted')))"
    )
}

fn scope(s: &GithubScope) -> Result<String, StorageError> {
    if s.repositories.len() > usize::from(MAX_PAGE) {
        return Err(invalid("GitHub repository scope batch exceeds500"));
    }
    encode(&s.repositories)
}

fn account(c: &Connection) -> Result<Option<StoredGithubAccount>, StorageError> {
    by_external(c, "GithubAccount", "github.com")
}
fn repository(c: &Connection, id: &Id) -> Result<Option<StoredGithubRepository>, StorageError> {
    let Some(b) = binding(c, id)? else {
        return Ok(None);
    };
    let live:bool=db(c.query_row("SELECT EXISTS(SELECT 1 FROM repositories r LEFT JOIN repository_details d ON d.repository_id=r.id WHERE r.id=?1 AND coalesce(d.lifecycle,'Active')!='Deleted' AND d.remote_url=?2)",params![id.as_str(),b.registered_remote],|r|r.get(0)))?;
    if b.state != "Active" || !live {
        return Ok(None);
    }
    record(c, "GithubRepository", b.github_repository_id.as_str())
}

fn observation(c: &Connection, id: &Id) -> Result<Option<StoredGithubObservation>, StorageError> {
    record(c, "GithubObservation", id.as_str())
}
fn issue(
    c: &Connection,
    k: &GithubIssueKey,
) -> Result<Option<StoredGithubIssueIntake>, StorageError> {
    by_external(c, "GithubIssueIntake", &key(k)?)
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
    let ok:bool=db(c.query_row(&format!("SELECT EXISTS(SELECT 1 {BINDING_NATIVE} JOIN project_repositories m ON m.repository_id=binding.repository_id JOIN projects p ON p.id=m.project_id WHERE binding.kind='GithubRepositoryBinding' AND binding.state='Active' AND coalesce(details.lifecycle,'Active')!='Deleted' AND details.remote_url=json_extract(binding.body_json,'$.registered_remote') AND json_extract(binding.body_json,'$.github_repository_id')=?2 AND m.project_id=?1 AND p.lifecycle!='Deleted')"),params![project,repo.as_str()],|r|r.get(0)))?;
    if ok {
        Ok(())
    } else {
        Err(invalid(
            "GitHub work project lacks a current registered source binding",
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
        "Repository"
            | "Issue"
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
    ) || (r.work_id.is_some() && r.project_id.is_none())
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
    if let Some(project) = &r.project_id {
        member(c, project.0.as_str(), &r.key.repository_id)?;
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
    let id = format!("github-intake-{}", hash(&key(&r.key)?)?);
    let exists: bool = db(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM integration_records WHERE id=?1)",
        [&id],
        |r| r.get(0),
    ))?;
    if exists {
        db(c.execute("UPDATE integration_records SET revision=?2,state=?3,project_id=?4,root_id=?5,body_json=?6 WHERE id=?1 AND kind='GithubIssueIntake'",params![id,r.revision,r.state,r.project_id.as_ref().map(|p|p.0.as_str()),r.work_id.as_ref().map(|w|w.0.as_str()),encode(r)?]))?;
        Ok(())
    } else {
        insert(
            c,
            &id,
            "GithubIssueIntake",
            &key(&r.key)?,
            None,
            r.project_id.as_ref(),
            r.work_id.as_ref(),
            r.revision,
            &r.state,
            r,
        )
    }
}

macro_rules! reads{($t:ty)=>{impl GithubRead for $t{
 fn github_adjudication_owner(&self,artifact:&Id,version:Option<&Id>)->Result<Option<StoredGithubObligationRetirement>,StorageError>{let v:Option<String>=db(self.conn.query_row("SELECT body_json FROM integration_records WHERE kind='GithubObligationRetirement' AND json_extract(body_json,'$.adjudication_ref.artifactId')=?1 AND (?2 IS NULL OR json_extract(body_json,'$.adjudication_ref.versionId')=?2) ORDER BY id LIMIT 1",params![artifact.as_str(),version.map(Id::as_str)],|r|r.get(0)).optional())?;v.map(decode).transpose()}

 fn github_related_work(&self,k:&GithubIssueKey,project:Option<&ProjectId>,after:Option<&RootId>,limit:u16)->Result<Vec<GithubWorkLink>,StorageError>{related_work(self.conn,k,project,after,limit)}
 fn github_obligation_retirements(&self,k:&GithubIssueKey,project:Option<&ProjectId>,after:Option<&Id>,limit:u16)->Result<Vec<StoredGithubObligationRetirement>,StorageError>{let n=page(limit)?;let mut q=db(self.conn.prepare_cached("SELECT body_json FROM integration_records WHERE kind='GithubObligationRetirement' AND json_extract(body_json,'$.key.repository_id')=?1 AND json_extract(body_json,'$.key.number')=?2 AND (?3 IS NULL OR project_id=?3) AND (?4 IS NULL OR id>?4) ORDER BY id LIMIT ?5"))?;let rows=db(db(q.query_map(params![k.repository_id.as_str(),k.number,project.map(|p|p.0.as_str()),after.map(Id::as_str),n],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(decode).collect()}

 fn github_repository_by_remote_id(&self,remote:&str)->Result<Option<StoredGithubRepository>,StorageError>{by_external(self.conn,"GithubRepository",remote)}
 fn github_repository_binding(&self,id:&Id)->Result<Option<StoredGithubRepositoryBinding>,StorageError>{binding_projection(self.conn,id)}
 fn github_repository_binding_history(&self,id:&Id,after:Option<i64>,limit:u16)->Result<Vec<StoredGithubRepositoryBinding>,StorageError>{let n=page(limit)?;let mut q=db(self.conn.prepare_cached("SELECT body_json FROM integration_records WHERE kind='GithubRepositoryBindingHistory' AND repository_id=?1 AND revision>coalesce(?2,0) ORDER BY revision LIMIT ?3"))?;let rows=db(db(q.query_map(params![id.as_str(),after,n],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(decode).collect()}
 fn github_discovered_repository(&self,id:&Id)->Result<Option<StoredGithubRepository>,StorageError>{record(self.conn,"GithubRepository",id.as_str())}
 fn github_account_repositories(&self,account:&Id,owner:Option<&str>,after:Option<&Id>,limit:u16)->Result<Vec<StoredGithubRepository>,StorageError>{let n=page(limit)?;let mut q=db(self.conn.prepare_cached("SELECT body_json FROM integration_records WHERE kind='GithubRepository' AND json_extract(body_json,'$.account_id')=?1 AND (?2 IS NULL OR json_extract(body_json,'$.owner')=?2 COLLATE NOCASE) AND (?3 IS NULL OR id>?3) ORDER BY id LIMIT ?4"))?;let rows=db(db(q.query_map(params![account.as_str(),owner,after.map(Id::as_str),n],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(decode).collect()}
 fn github_account(&self)->Result<Option<StoredGithubAccount>,StorageError>{account(self.conn)}
 fn github_repository(&self,id:&Id)->Result<Option<StoredGithubRepository>,StorageError>{repository(self.conn,id)}
 fn github_repositories(&self,s:&GithubScope,after:Option<&Id>,limit:u16)->Result<Vec<StoredGithubRepository>,StorageError>{let n=page(limit)?;let allowed=canonical_scope("r.id");let mut q=db(self.conn.prepare_cached(&format!("SELECT r.body_json FROM integration_records r WHERE r.kind='GithubRepository' AND {allowed} AND (?3 IS NULL OR r.id>?3) ORDER BY r.id LIMIT ?4")))?;let rows=db(db(q.query_map(params![scope(s)?,s.project_id.as_ref().map(|p|p.0.as_str()),after.map(Id::as_str),n],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(decode).collect()}
 fn github_observation(&self,id:&Id,s:&GithubScope)->Result<Option<StoredGithubObservation>,StorageError>{let allowed=canonical_scope("json_extract(r.body_json,'$.repository_id')");let v:Option<String>=db(self.conn.query_row(&format!("SELECT r.body_json FROM integration_records r WHERE r.id=?3 AND r.kind='GithubObservation' AND {allowed}"),params![scope(s)?,s.project_id.as_ref().map(|p|p.0.as_str()),id.as_str()],|r|r.get(0)).optional())?;v.map(decode).transpose()}
 fn github_observations(&self,f:&GithubObservationFilter,after:Option<&Id>,limit:u16)->Result<Vec<StoredGithubObservation>,StorageError>{let n=page(limit)?;let allowed=canonical_scope("json_extract(r.body_json,'$.repository_id')");let mut q=db(self.conn.prepare_cached(&format!("SELECT r.body_json FROM integration_records r WHERE r.kind='GithubObservation' AND {allowed} AND (?3 IS NULL OR r.id>?3) AND (json_array_length(?4)=0 OR json_extract(r.body_json,'$.kind') IN(SELECT value FROM json_each(?4))) AND (?5 IS NULL OR json_extract(r.body_json,'$.subject_id')=?5) AND (?6 IS NULL OR json_extract(r.body_json,'$.head')=?6) ORDER BY r.id LIMIT ?7")))?;let rows=db(db(q.query_map(params![scope(&f.scope)?,f.scope.project_id.as_ref().map(|p|p.0.as_str()),after.map(Id::as_str),encode(&f.kinds)?,f.subject_id,f.head,n],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(decode).collect()}
 fn github_poll_cursor(&self,r:&Id,e:&str)->Result<Option<StoredGithubPollCursor>,StorageError>{poll(self.conn,r,e)}
 fn github_issue(&self,k:&GithubIssueKey,s:&GithubScope)->Result<Option<StoredGithubIssueIntake>,StorageError>{key(k)?;let allowed=canonical_scope("json_extract(r.body_json,'$.key.repository_id')");let v:Option<String>=db(self.conn.query_row(&format!("SELECT r.body_json FROM integration_records r WHERE r.external_key=?3 AND r.kind='GithubIssueIntake' AND {allowed} AND (?2 IS NULL OR r.project_id IS NULL OR r.project_id=?2)"),params![scope(s)?,s.project_id.as_ref().map(|p|p.0.as_str()),key(k)?],|r|r.get(0)).optional())?;v.map(decode).transpose()}
 fn github_issues(&self,f:&GithubIntakeFilter,after:Option<&GithubIssueKey>,limit:u16)->Result<Vec<StoredGithubIssueIntake>,StorageError>{let n=page(limit)?;if let Some(k)=after{key(k)?;}let allowed=canonical_scope("json_extract(r.body_json,'$.key.repository_id')");let mut q=db(self.conn.prepare_cached(&format!("SELECT r.body_json FROM integration_records r WHERE r.kind='GithubIssueIntake' AND {allowed} AND (?2 IS NULL OR r.project_id IS NULL OR r.project_id=?2) AND (?3 IS NULL OR (json_extract(r.body_json,'$.key.repository_id'),json_extract(r.body_json,'$.key.number'))>(?3,?4)) AND (json_array_length(?5)=0 OR r.state IN(SELECT value FROM json_each(?5))) ORDER BY json_extract(r.body_json,'$.key.repository_id'),json_extract(r.body_json,'$.key.number') LIMIT ?6")))?;let rows=db(db(q.query_map(params![scope(&f.scope)?,f.scope.project_id.as_ref().map(|p|p.0.as_str()),after.map(|k|k.repository_id.as_str()),after.map(|k|k.number),encode(&f.states)?,n],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(decode).collect()}
 fn github_intake_clock(&self)->Result<GithubIntakeClock,StorageError>{clock(self.conn)}
 fn github_obligation_revision(&self,k:&GithubIssueKey)->Result<i64,StorageError>{obligation_revision(self.conn,k)}
 fn github_work_links(&self,k:&GithubIssueKey,project:Option<&ProjectId>,after:Option<&RootId>,limit:u16)->Result<Vec<GithubWorkLink>,StorageError>{let n=page(limit)?;let mut q=db(self.conn.prepare_cached("SELECT w.id,w.project_id FROM integration_links l JOIN work_items w ON w.id=l.work_id WHERE l.record_id=?1 AND l.purpose='GithubIssue' AND (?2 IS NULL OR w.project_id=?2) AND (?3 IS NULL OR w.id>?3) ORDER BY w.id LIMIT ?4"))?;let rows=db(db(q.query_map(params![source_id(k)?,project.map(|p|p.0.as_str()),after.map(|w|w.0.as_str()),n],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(|(w,p)|Ok(GithubWorkLink{work_id:RootId(Id::new(w).map_err(|_|invalid("work identity"))?),project_id:ProjectId(Id::new(p).map_err(|_|invalid("project identity"))?)})).collect()}
 fn github_resolution(&self,id:&Id)->Result<Option<StoredGithubResolution>,StorageError>{resolution(self.conn,id)}
 fn github_resolutions(&self,k:&GithubIssueKey,after:Option<&Id>,limit:u16)->Result<Vec<StoredGithubResolution>,StorageError>{let n=page(limit)?;let mut q=db(self.conn.prepare_cached("SELECT body_json FROM integration_records WHERE kind='GithubResolution' AND json_extract(body_json,'$.key.repository_id')=?1 AND json_extract(body_json,'$.key.number')=?2 AND (?3 IS NULL OR id>?3) ORDER BY id LIMIT ?4"))?;let rows=db(db(q.query_map(params![k.repository_id.as_str(),k.number,after.map(Id::as_str),n],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(decode).collect()}
 fn github_resolution_witnesses(&self,id:&Id,project:Option<&ProjectId>,after:Option<&RootId>,limit:u16)->Result<Vec<StoredGithubResolutionWitness>,StorageError>{let n=page(limit)?;let mut q=db(self.conn.prepare_cached("SELECT body_json FROM integration_records WHERE kind='GithubResolutionWitness' AND json_extract(body_json,'$.resolution_id')=?1 AND (?2 IS NULL OR project_id=?2) AND (?3 IS NULL OR root_id>?3) ORDER BY root_id LIMIT ?4"))?;let rows=db(db(q.query_map(params![id.as_str(),project.map(|p|p.0.as_str()),after.map(|w|w.0.as_str()),n],|r|r.get::<_,String>(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;rows.into_iter().map(decode).collect()}
}};}
reads!(Snapshot<'_>);
reads!(Unit<'_>);
impl GithubRepository for Unit<'_> {
    fn retire_github_work_link(
        &mut self,
        r: &StoredGithubObligationRetirement,
    ) -> Result<(), StorageError> {
        let external = format!(
            "{}:{}:{}",
            key(&r.key)?,
            r.work_id.0.as_str(),
            r.obligation_revision
        );
        if let Some(old) = by_external::<StoredGithubObligationRetirement>(
            self.conn,
            "GithubObligationRetirement",
            &external,
        )? {
            return if old == *r {
                Ok(())
            } else {
                Err(invalid("immutable GitHub adjudication changed"))
            };
        }
        if issue(self.conn, &r.key)?.is_none_or(|source| source.observation_id != r.observation_id)
        {
            return Err(invalid("GitHub adjudication remote snapshot changed"));
        }
        let (project, revision) = work(self.conn, &r.work_id)?;
        if project != r.project_id.0.as_str() || revision != r.work_revision {
            return Err(invalid("GitHub adjudication work scope/revision changed"));
        }
        let current = obligation_revision(self.conn, &r.key)?;
        if current != r.obligation_revision {
            return Err(conflict(
                &source_id(&r.key)?,
                r.obligation_revision,
                current,
            ));
        }
        let linked:bool=db(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM integration_links WHERE record_id=?1 AND work_id=?2 AND purpose='GithubIssue')",params![source_id(&r.key)?,r.work_id.0.as_str()],|r|r.get(0)))?;
        if !linked {
            return Err(invalid("GitHub current obligation missing"));
        }
        let op = self
            .operation(&r.operation_id)?
            .ok_or_else(|| invalid("GitHub adjudication operation missing"))?;
        if !matches!(
            op.capability.as_str(),
            "github.issue_sync" | "github.triage"
        ) || op.state != OperationState::ObservedComplete
            || op.payload["repositoryId"].as_str() != Some(r.key.repository_id.as_str())
            || op.payload["issueNumber"].as_i64() != Some(r.key.number)
            || op.payload["actorId"].as_str() != Some(r.actor_id.0.as_str())
            || r.created_at_ms < 0
        {
            return Err(invalid(
                "GitHub adjudication lacks its actual completed service operation",
            ));
        }
        adjudication_backing(self.conn, r, &self.info.host_actor)?;
        let next = current
            .checked_add(1)
            .ok_or_else(|| invalid("GitHub obligation revision overflow"))?;
        atomic(self.conn, || {
            insert(
                self.conn,
                r.id.as_str(),
                "GithubObligationRetirement",
                &external,
                None,
                Some(&r.project_id),
                Some(&r.work_id),
                1,
                "Recorded",
                r,
            )?;
            db(self.conn.execute("INSERT INTO integration_links(record_id,work_id,purpose) VALUES(?1,?2,'GithubIssueHistory') ON CONFLICT DO NOTHING",params![source_id(&r.key)?,r.work_id.0.as_str()]))?;
            db(self.conn.execute("DELETE FROM integration_links WHERE record_id=?1 AND work_id=?2 AND purpose='GithubIssue'",params![source_id(&r.key)?,r.work_id.0.as_str()]))?;
            db(self.conn.execute(
                "UPDATE integration_records SET revision=?2 WHERE id=?1",
                params![source_id(&r.key)?, next],
            ))?;
            Ok(())
        })
    }

    fn put_github_repository_binding(
        &mut self,
        r: &StoredGithubRepositoryBinding,
        expected: Option<i64>,
    ) -> Result<(), StorageError> {
        let old = binding(self.conn, &r.repository_id)?;
        cas(
            r.revision,
            expected,
            old.as_ref().map(|r| r.revision),
            r.repository_id.as_str(),
        )?;
        let (revision,lifecycle,remote):(i64,String,Option<String>)=db(self.conn.query_row("SELECT n.revision,coalesce(d.lifecycle,'Active'),d.remote_url FROM repositories n LEFT JOIN repository_details d ON d.repository_id=n.id WHERE n.id=?1",[r.repository_id.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))))?;
        if !matches!(r.state.as_str(), "Active" | "Detached")
            || r.repository_revision != revision
            || remote.as_deref() != Some(r.registered_remote.as_str())
        {
            return Err(invalid(
                "GitHub checkout binding differs from actual current registration",
            ));
        }
        if r.state == "Active" {
            if lifecycle == "Deleted" {
                return Err(invalid("deleted checkout cannot grant GitHub scope"));
            }
            let canonical: StoredGithubRepository = record(
                self.conn,
                "GithubRepository",
                r.github_repository_id.as_str(),
            )?
            .ok_or_else(|| invalid("GitHub observed identity missing"))?;
            let observation = observation(self.conn, &r.observation_id)?
                .ok_or_else(|| invalid("GitHub server identity observation missing"))?;
            let actual =
                devworks_core::services::git::github::Repository::from_remote(&r.registered_remote)
                    .ok_or_else(|| invalid("registered remote is not qualified GitHub.com"))?;
            let requested = observation.payload["requestedRemote"]
                .as_str()
                .and_then(devworks_core::services::git::github::Repository::from_remote)
                .ok_or_else(|| invalid("identity observation has no actual requested remote"))?;
            let response = &observation.payload["repository"];
            let remote_id = response["node_id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| response["id"].as_i64().map(|id| id.to_string()));
            if observation.kind != "Repository"
                || observation.repository_id != r.github_repository_id
                || observation.subject_id != canonical.remote_repository_id
                || remote_id.as_deref() != Some(canonical.remote_repository_id.as_str())
                || !actual.owner.eq_ignore_ascii_case(&requested.owner)
                || !actual.name.eq_ignore_ascii_case(&requested.name)
                || response["owner"]["login"].as_str() != Some(canonical.owner.as_str())
                || response["name"].as_str() != Some(canonical.name.as_str())
            {
                return Err(invalid(
                    "GitHub binding lacks exact observed server identity/current source association",
                ));
            }
        } else if old.as_ref().is_none_or(|o| {
            o.github_repository_id != r.github_repository_id || o.observation_id != r.observation_id
        }) {
            return Err(invalid("detaching a source cannot invent a new identity"));
        }
        let external = r.repository_id.as_str();
        let current_id = format!("github-binding-{}", hash(&r.repository_id)?);
        atomic(self.conn, || {
            let history = format!("{}:{}", external, r.revision);
            insert(
                self.conn,
                &format!("github-binding-history-{}", hash(&history)?),
                "GithubRepositoryBindingHistory",
                &history,
                Some(&r.repository_id),
                None,
                None,
                r.revision,
                &r.state,
                r,
            )?;
            if old.is_some() {
                update(
                    self.conn,
                    &current_id,
                    "GithubRepositoryBinding",
                    r.revision,
                    &r.state,
                    r,
                )
            } else {
                insert(
                    self.conn,
                    &current_id,
                    "GithubRepositoryBinding",
                    external,
                    Some(&r.repository_id),
                    None,
                    None,
                    r.revision,
                    &r.state,
                    r,
                )
            }
        })
    }

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
            return Err(invalid("GitHub repository identity mismatch"));
        }
        if old.is_some() {
            update(
                self.conn,
                r.id.as_str(),
                "GithubRepository",
                r.revision,
                "Observed",
                r,
            )
        } else {
            insert(
                self.conn,
                r.id.as_str(),
                "GithubRepository",
                &r.remote_repository_id,
                None,
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
            None,
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
                || r.work_id.is_some()
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
                    None,
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
        if seconds == c.interval_seconds {
            return Ok(c);
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
        if let Some(previous) = &r.operation_id
            && self
                .operation(previous)?
                .is_none_or(|o| !is_terminal(o.state))
        {
            return Err(StorageError::Busy);
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
            "SELECT EXISTS(SELECT 1 FROM integration_records WHERE kind='GithubIssueIntake' AND json_extract(body_json,'$.operation_id')=?1)",
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
                db(self.conn.execute("INSERT INTO integration_links(record_id,work_id,purpose) VALUES(?1,?2,'GithubIssueHistory') ON CONFLICT DO NOTHING",params![sid,w.0.as_str()]))?;
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
        if let Some(old) =
            by_external::<StoredGithubResolution>(self.conn, "GithubResolution", &external)?
        {
            if old.key != r.key
                || old.source_revision != r.source_revision
                || old.obligation_revision != r.obligation_revision
                || old.summary_digest != r.summary_digest
            {
                return Err(invalid("immutable GitHub completion binding changed"));
            }
            return Ok(old);
        }
        let active:bool=db(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM integration_records r JOIN operations comment ON comment.id=json_extract(r.body_json,'$.comment_operation_id') JOIN operations close ON close.id=json_extract(r.body_json,'$.close_operation_id') WHERE r.kind='GithubResolution' AND json_extract(r.body_json,'$.key.repository_id')=?1 AND json_extract(r.body_json,'$.key.number')=?2 AND (comment.state NOT IN('ObservedComplete','Failed','CancelledBeforeDispatch') OR close.state NOT IN('ObservedComplete','Failed','CancelledBeforeDispatch')))",params![r.key.repository_id.as_str(),r.key.number],|r|r.get(0)))?;
        if active {
            return Err(StorageError::Busy);
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
            None,
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
            None,
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

fn related_work(
    c: &Connection,
    k: &GithubIssueKey,
    project: Option<&ProjectId>,
    after: Option<&RootId>,
    limit: u16,
) -> Result<Vec<GithubWorkLink>, StorageError> {
    let n = page(limit)?;
    let mut q=db(c.prepare_cached("SELECT w.id,w.project_id FROM integration_links l JOIN work_items w ON w.id=l.work_id WHERE l.record_id=?1 AND l.purpose='GithubIssueHistory' AND (?2 IS NULL OR w.project_id=?2) AND (?3 IS NULL OR w.id>?3) ORDER BY w.id LIMIT ?4"))?;
    let rows = db(db(q.query_map(
        params![
            source_id(k)?,
            project.map(|p| p.0.as_str()),
            after.map(|w| w.0.as_str()),
            n
        ],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    ))?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter()
        .map(|(w, p)| {
            Ok(GithubWorkLink {
                work_id: RootId(Id::new(w).map_err(|_| invalid("work identity"))?),
                project_id: ProjectId(Id::new(p).map_err(|_| invalid("project identity"))?),
            })
        })
        .collect()
}
fn adjudication_backing(
    c: &Connection,
    r: &StoredGithubObligationRetirement,
    host: &devworks_core::protocol::ActorId,
) -> Result<(), StorageError> {
    use devworks_core::protocol::{Check, provider::ArtifactRef};
    let reference: ArtifactRef = serde_json::from_value(r.adjudication_ref.clone())
        .map_err(|_| invalid("invalid GitHub adjudication source ref"))?;
    reference
        .check()
        .map_err(|_| invalid("invalid adjudication ref"))?;
    type AdjudicationBacking = (String, Option<String>, String, i64, String, String, String);
    let row:Option<AdjudicationBacking>=db(c.query_row("SELECT a.provenance_json,a.owner_actor_id,v.blob_digest,b.byte_count,v.media_type,v.artifact_id,v.source_json FROM artifact_versions v JOIN artifacts a ON a.id=v.artifact_id JOIN blobs b ON b.digest=v.blob_digest WHERE v.id=?1 AND v.deleted_at IS NULL AND a.state='Live' AND b.state='Present' AND (a.current_project_id IS NULL OR a.current_project_id=?2)",params![reference.version_id.as_str(),r.project_id.0.as_str()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional())?;
    let Some((provenance, owner, sha, bytes, media, artifact, version_source)) = row else {
        return Err(invalid("GitHub adjudication backing missing/outside scope"));
    };
    let provenance: serde_json::Value = decode(provenance)?;
    let derived = &provenance["derived"];
    let version_source: serde_json::Value = decode(version_source)?;
    if artifact != reference.artifact_id.as_str()
        || sha != reference.sha256.as_str()
        || bytes != reference.byte_length.get()
        || media != reference.media_type
        || owner.as_deref() != Some(host.0.as_str())
        || derived["actorId"].as_str() != Some(r.actor_id.0.as_str())
        || version_source["producedBy"].as_str() != Some(host.0.as_str())
        || version_source["principalKind"] != "Host"
        || version_source["source"]["actorId"].as_str() != Some(r.actor_id.0.as_str())
        || version_source["source"]["operationId"].as_str() != Some(r.operation_id.0.as_str())
        || version_source["source"]["observationId"].as_str() != Some(r.observation_id.as_str())
        || provenance["principalKind"] != "Host"
        || provenance["producedBy"].as_str() != Some(host.0.as_str())
        || derived["kind"] != "GithubObligationAdjudication"
        || derived["repositoryId"].as_str() != Some(r.key.repository_id.as_str())
        || derived["issueNumber"].as_i64() != Some(r.key.number)
        || derived["workId"].as_str() != Some(r.work_id.0.as_str())
        || derived["workRevision"].as_i64() != Some(r.work_revision)
        || derived["obligationRevision"].as_i64() != Some(r.obligation_revision)
        || derived["operationId"].as_str() != Some(r.operation_id.0.as_str())
        || derived["observationId"].as_str() != Some(r.observation_id.as_str())
        || derived["reason"]
            != serde_json::to_value(&r.reason).map_err(|_| invalid("GitHub adjudication reason"))?
    {
        return Err(invalid(
            "GitHub adjudication source does not bind actual work/obligation/operation",
        ));
    }
    Ok(())
}
