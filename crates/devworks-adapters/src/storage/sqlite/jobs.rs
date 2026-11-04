//! SQLite schedules and runs (migrations 1 and 19): `schedules`, `job_runs`, `job_run_links`,
//! `job_checkpoints`, `job_actions` and `job_run_usage`.
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{
    ActorId, Id, OperationId, ProjectId, Revision, RevisionDimension, RootId,
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
fn limit_ok(limit: u16) -> Result<i64, StorageError> {
    if limit == 0 || limit > 500 {
        return Err(invalid("limit must be 1..=500"));
    }
    Ok(i64::from(limit))
}
fn conflict(kind: &'static str, id: &Id, expected: i64, current: i64) -> StorageError {
    StorageError::conflict(
        RevisionDimension::Schedule,
        kind,
        id.clone(),
        Revision::new(expected.max(1)).expect("positive"),
        Revision::new(current.max(1)).expect("positive"),
    )
}
fn duplicate(kind: &'static str, id: impl Into<String>) -> StorageError {
    StorageError::Duplicate {
        kind,
        id: id.into(),
    }
}
fn json_ok(s: &str, what: &str) -> Result<(), StorageError> {
    serde_json::from_str::<serde_json::Value>(s)
        .map(|_| ())
        .map_err(|_| invalid(format!("{what} must be JSON")))
}

// ---------------------------------------------------------------------------------------------
// Jobs

const JOB_COLUMNS: &str = "id,revision,project_id,title,kind,schedule_json,timezone,definition_json,enabled,authorized,authorized_by,hold,activated_epoch,anchor_at,next_at,interrupted_run_id,created_at,updated_at,deleted_at,project_epoch";

fn job_row(r: &Row<'_>) -> rusqlite::Result<[rusqlite::types::Value; 20]> {
    let mut out: [rusqlite::types::Value; 20] =
        std::array::from_fn(|_| rusqlite::types::Value::Null);
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = r.get(i)?;
    }
    Ok(out)
}
fn text(v: &rusqlite::types::Value) -> Result<Option<String>, StorageError> {
    match v {
        rusqlite::types::Value::Null => Ok(None),
        rusqlite::types::Value::Text(s) => Ok(Some(s.clone())),
        _ => Err(corrupt("stored job text")),
    }
}
fn need_text(v: &rusqlite::types::Value) -> Result<String, StorageError> {
    text(v)?.ok_or_else(|| corrupt("stored job text missing"))
}
fn int(v: &rusqlite::types::Value) -> Result<Option<i64>, StorageError> {
    match v {
        rusqlite::types::Value::Null => Ok(None),
        rusqlite::types::Value::Integer(n) => Ok(Some(*n)),
        _ => Err(corrupt("stored job integer")),
    }
}
fn need_int(v: &rusqlite::types::Value) -> Result<i64, StorageError> {
    int(v)?.ok_or_else(|| corrupt("stored job integer missing"))
}
fn job_of(r: [rusqlite::types::Value; 20]) -> Result<StoredJob, StorageError> {
    let hold = text(&r[11])?;
    if hold.as_deref().is_some_and(|h| !JOB_HOLDS.contains(&h)) {
        return Err(corrupt("stored job hold"));
    }
    Ok(StoredJob {
        id: id_of(need_text(&r[0])?)?,
        revision: need_int(&r[1])?,
        project_id: opt_id(text(&r[2])?)?.map(ProjectId),
        title: need_text(&r[3])?,
        kind: need_text(&r[4])?,
        schedule: need_text(&r[5])?,
        timezone: need_text(&r[6])?,
        definition: need_text(&r[7])?,
        enabled: need_int(&r[8])? != 0,
        authorized: need_int(&r[9])? != 0,
        authorized_by: opt_id(text(&r[10])?)?.map(ActorId),
        hold,
        activated_epoch: need_int(&r[12])?,
        anchor_at_ms: need_int(&r[13])?,
        next_at_ms: int(&r[14])?,
        interrupted_run_id: opt_id(text(&r[15])?)?,
        created_at_ms: need_int(&r[16])?,
        updated_at_ms: need_int(&r[17])?,
        deleted_at_ms: int(&r[18])?,
        project_epoch: need_int(&r[19])?,
    })
}
fn jobs_query(
    conn: &Connection,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<StoredJob>, StorageError> {
    let mut st = db(conn.prepare_cached(sql))?;
    let rows = db(st
        .query_map(args, job_row)
        .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>()))?;
    rows.into_iter().map(job_of).collect()
}
fn job(conn: &Connection, id: &Id) -> Result<Option<StoredJob>, StorageError> {
    db(conn
        .query_row(
            &format!("SELECT {JOB_COLUMNS} FROM schedules WHERE id=?1"),
            params![id.as_str()],
            job_row,
        )
        .optional())?
    .map(job_of)
    .transpose()
}
fn validate_job(j: &StoredJob) -> Result<(), StorageError> {
    if j.title.trim().is_empty() || j.title.chars().count() > 256 {
        return Err(invalid("a job title is 1-256 characters"));
    }
    if !["Native", "Inference"].contains(&j.kind.as_str()) {
        return Err(invalid("unknown job kind"));
    }
    if j.hold.as_deref().is_some_and(|h| !JOB_HOLDS.contains(&h)) {
        return Err(invalid("unknown job hold"));
    }
    json_ok(&j.schedule, "schedule")?;
    json_ok(&j.definition, "definition")?;
    if j.timezone.is_empty() {
        return Err(invalid("a job names its time zone"));
    }
    if j.deleted_at_ms.is_some() && (j.enabled || j.next_at_ms.is_some()) {
        return Err(invalid("a deleted job is disabled and schedules nothing"));
    }
    if !j.enabled && j.next_at_ms.is_some() {
        return Err(invalid("a disabled job schedules nothing"));
    }
    Ok(())
}
fn title_taken(conn: &Connection, j: &StoredJob) -> Result<bool, StorageError> {
    if j.deleted_at_ms.is_some() {
        return Ok(false);
    }
    let other: Option<String> = db(conn
        .query_row(
            "SELECT id FROM schedules WHERE ifnull(project_id,'')=ifnull(?1,'') AND title=?2 COLLATE NOCASE AND deleted_at IS NULL AND id<>?3",
            params![
                j.project_id.as_ref().map(|p| p.0.as_str()),
                j.title,
                j.id.as_str()
            ],
            |r| r.get(0),
        )
        .optional())?;
    Ok(other.is_some())
}
fn write_job(conn: &Connection, j: &StoredJob, revision: i64) -> Result<(), StorageError> {
    db(conn.execute(
        "INSERT INTO schedules(id,revision,project_id,title,kind,schedule_json,timezone,definition_json,enabled,authorized,authorized_by,hold,activated_epoch,anchor_at,next_at,interrupted_run_id,created_at,updated_at,deleted_at,project_epoch) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20) \
         ON CONFLICT(id) DO UPDATE SET revision=excluded.revision, title=excluded.title, kind=excluded.kind, \
         schedule_json=excluded.schedule_json, timezone=excluded.timezone, definition_json=excluded.definition_json, \
         enabled=excluded.enabled, authorized=excluded.authorized, authorized_by=excluded.authorized_by, hold=excluded.hold, \
         activated_epoch=excluded.activated_epoch, anchor_at=excluded.anchor_at, next_at=excluded.next_at, \
         interrupted_run_id=excluded.interrupted_run_id, \
         updated_at=excluded.updated_at, deleted_at=excluded.deleted_at, project_epoch=excluded.project_epoch",
        params![
            j.id.as_str(),
            revision,
            j.project_id.as_ref().map(|p| p.0.as_str()),
            j.title,
            j.kind,
            j.schedule,
            j.timezone,
            j.definition,
            j.enabled,
            j.authorized,
            j.authorized_by.as_ref().map(|a| a.0.as_str()),
            j.hold,
            j.activated_epoch,
            j.anchor_at_ms,
            j.next_at_ms,
            j.interrupted_run_id.as_ref().map(Id::as_str),
            j.created_at_ms,
            j.updated_at_ms,
            j.deleted_at_ms,
            j.project_epoch,
        ],
    ))?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Runs

const RUN_COLUMNS: &str = "id,schedule_id,seq,project_id,job_revision,definition_json,occurrence_key,occurrence_at,trigger,state,revision,operation_id,inner_operation_id,boot_id,started_at,finished_at,reason,hold,cancel_requested,resumptions,session_id,agent_actor,skipped_for,checkpoint_json,created_at,project_epoch,restore_epoch";

fn run_row(r: &Row<'_>) -> rusqlite::Result<[rusqlite::types::Value; 27]> {
    let mut out: [rusqlite::types::Value; 27] =
        std::array::from_fn(|_| rusqlite::types::Value::Null);
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = r.get(i)?;
    }
    Ok(out)
}
fn run_of(r: [rusqlite::types::Value; 27]) -> Result<StoredJobRun, StorageError> {
    let status = need_text(&r[9])?;
    if !RUN_STATUSES.contains(&status.as_str()) {
        return Err(corrupt("stored run status"));
    }
    Ok(StoredJobRun {
        id: id_of(need_text(&r[0])?)?,
        job_id: id_of(need_text(&r[1])?)?,
        seq: need_int(&r[2])?,
        project_id: opt_id(text(&r[3])?)?.map(ProjectId),
        job_revision: need_int(&r[4])?,
        definition: need_text(&r[5])?,
        occurrence_key: need_text(&r[6])?,
        occurrence_at_ms: need_int(&r[7])?,
        trigger: need_text(&r[8])?,
        status,
        revision: need_int(&r[10])?,
        operation_id: opt_id(text(&r[11])?)?.map(OperationId),
        inner_operation_id: opt_id(text(&r[12])?)?.map(OperationId),
        boot_id: opt_id(text(&r[13])?)?,
        started_at_ms: int(&r[14])?,
        finished_at_ms: int(&r[15])?,
        reason: text(&r[16])?,
        hold: text(&r[17])?,
        cancel_requested: need_int(&r[18])? != 0,
        resumptions: need_int(&r[19])?,
        session_id: opt_id(text(&r[20])?)?,
        agent_actor: opt_id(text(&r[21])?)?.map(ActorId),
        skipped_for: opt_id(text(&r[22])?)?,
        checkpoint: text(&r[23])?,
        created_at_ms: need_int(&r[24])?,
        project_epoch: need_int(&r[25])?,
        restore_epoch: need_int(&r[26])?,
    })
}
fn runs_query(
    conn: &Connection,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<StoredJobRun>, StorageError> {
    let mut st = db(conn.prepare_cached(sql))?;
    let rows = db(st
        .query_map(args, run_row)
        .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>()))?;
    rows.into_iter().map(run_of).collect()
}
fn run(conn: &Connection, id: &Id) -> Result<Option<StoredJobRun>, StorageError> {
    Ok(runs_query(
        conn,
        &format!("SELECT {RUN_COLUMNS} FROM job_runs WHERE id=?1"),
        &[&id.as_str()],
    )?
    .pop())
}
fn active_run(conn: &Connection, job: &Id) -> Result<Option<StoredJobRun>, StorageError> {
    Ok(runs_query(
        conn,
        &format!(
            "SELECT {RUN_COLUMNS} FROM job_runs WHERE schedule_id=?1 AND state IN ('Queued','Running','Holding')"
        ),
        &[&job.as_str()],
    )?
    .pop())
}
fn validate_run(r: &StoredJobRun) -> Result<(), StorageError> {
    if !RUN_STATUSES.contains(&r.status.as_str()) {
        return Err(invalid("unknown run status"));
    }
    if !["Scheduled", "Manual"].contains(&r.trigger.as_str()) {
        return Err(invalid("unknown run trigger"));
    }
    if r.occurrence_key.is_empty() || r.occurrence_key.len() > 4096 {
        return Err(invalid("an occurrence key is 1-4096 bytes"));
    }
    json_ok(&r.definition, "run definition")?;
    if let Some(c) = &r.checkpoint {
        json_ok(c, "run checkpoint")?;
    }
    let terminal = !ACTIVE_RUN_STATUSES.contains(&r.status.as_str());
    if terminal != r.finished_at_ms.is_some() {
        return Err(invalid(
            "a run has a finish time exactly when it is finished",
        ));
    }
    if r.status != "Skipped" && r.operation_id.is_none() {
        return Err(invalid(
            "a run that is not a skipped occurrence has its operation",
        ));
    }
    if r.resumptions < 0 {
        return Err(invalid("negative resumptions"));
    }
    Ok(())
}
fn write_run(
    conn: &Connection,
    r: &StoredJobRun,
    seq: i64,
    revision: i64,
) -> Result<(), StorageError> {
    db(conn.execute(
        "INSERT INTO job_runs(id,schedule_id,seq,project_id,job_revision,definition_json,occurrence_key,occurrence_at,trigger,state,revision,operation_id,inner_operation_id,boot_id,started_at,finished_at,reason,hold,cancel_requested,resumptions,session_id,agent_actor,skipped_for,checkpoint_json,created_at,project_epoch,restore_epoch) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27) \
         ON CONFLICT(id) DO UPDATE SET state=excluded.state, revision=excluded.revision, operation_id=excluded.operation_id, \
         inner_operation_id=excluded.inner_operation_id, boot_id=excluded.boot_id, started_at=excluded.started_at, \
         finished_at=excluded.finished_at, reason=excluded.reason, hold=excluded.hold, cancel_requested=excluded.cancel_requested, \
         resumptions=excluded.resumptions, session_id=excluded.session_id, agent_actor=excluded.agent_actor, \
         checkpoint_json=excluded.checkpoint_json",
        params![
            r.id.as_str(),
            r.job_id.as_str(),
            seq,
            r.project_id.as_ref().map(|p| p.0.as_str()),
            r.job_revision,
            r.definition,
            r.occurrence_key,
            r.occurrence_at_ms,
            r.trigger,
            r.status,
            revision,
            r.operation_id.as_ref().map(|o| o.0.as_str()),
            r.inner_operation_id.as_ref().map(|o| o.0.as_str()),
            r.boot_id.as_ref().map(Id::as_str),
            r.started_at_ms,
            r.finished_at_ms,
            r.reason,
            r.hold,
            r.cancel_requested,
            r.resumptions,
            r.session_id.as_ref().map(Id::as_str),
            r.agent_actor.as_ref().map(|a| a.0.as_str()),
            r.skipped_for.as_ref().map(Id::as_str),
            r.checkpoint,
            r.created_at_ms,
            r.project_epoch,
            r.restore_epoch,
        ],
    ))?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Checkpoints, actions, usage

fn checkpoint(
    conn: &Connection,
    job: &Id,
    source: &str,
) -> Result<Option<StoredJobCheckpoint>, StorageError> {
    let raw: Option<(String, i64, String, i64)> = db(conn
        .query_row(
            "SELECT cursor_json,revision,run_id,updated_at FROM job_checkpoints WHERE schedule_id=?1 AND source=?2",
            params![job.as_str(), source],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional())?;
    raw.map(|(cursor, revision, run_id, updated_at_ms)| {
        Ok(StoredJobCheckpoint {
            job_id: job.clone(),
            source: source.to_owned(),
            cursor,
            revision,
            run_id: id_of(run_id)?,
            updated_at_ms,
        })
    })
    .transpose()
}
type RawAction = (
    String,
    String,
    String,
    String,
    Option<String>,
    i64,
    i64,
    Option<i64>,
);
fn action_of(r: RawAction) -> Result<StoredJobAction, StorageError> {
    if !ACTION_STATES.contains(&r.3.as_str()) {
        return Err(corrupt("stored action state"));
    }
    Ok(StoredJobAction {
        job_id: id_of(r.0)?,
        key: r.1,
        run_id: id_of(r.2)?,
        state: r.3,
        effect: r.4,
        revision: r.5,
        created_at_ms: r.6,
        settled_at_ms: r.7,
    })
}
const ACTION_COLUMNS: &str =
    "schedule_id,action_key,run_id,state,effect_json,revision,created_at,settled_at";
fn actions_query(
    conn: &Connection,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<StoredJobAction>, StorageError> {
    let mut st = db(conn.prepare_cached(sql))?;
    let rows: Vec<RawAction> = db(st
        .query_map(args, |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })
        .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>()))?;
    rows.into_iter().map(action_of).collect()
}
fn action(conn: &Connection, job: &Id, key: &str) -> Result<Option<StoredJobAction>, StorageError> {
    Ok(actions_query(
        conn,
        &format!("SELECT {ACTION_COLUMNS} FROM job_actions WHERE schedule_id=?1 AND action_key=?2"),
        &[&job.as_str(), &key],
    )?
    .pop())
}
fn validate_action(a: &StoredJobAction) -> Result<(), StorageError> {
    if a.key.is_empty() || a.key.len() > 512 {
        return Err(invalid("an action identity is 1-512 bytes"));
    }
    if !ACTION_STATES.contains(&a.state.as_str()) {
        return Err(invalid("unknown action state"));
    }
    if (a.state == "Intent") != a.settled_at_ms.is_none() {
        return Err(invalid(
            "an action is settled exactly when it is not an intent",
        ));
    }
    if let Some(e) = &a.effect {
        json_ok(e, "action effect")?;
    }
    Ok(())
}
const USAGE_COLUMNS: &str = "run_id,seq,request_id,purpose,model_id,input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,amount_usd,price_kind,price_origin,price_revision,incomplete,recorded_at";
fn usage_rows(
    conn: &Connection,
    run: &Id,
    after: i64,
    limit: i64,
) -> Result<Vec<StoredJobUsage>, StorageError> {
    let mut st = db(conn.prepare_cached(&format!(
        "SELECT {USAGE_COLUMNS} FROM job_run_usage WHERE run_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3"
    )))?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        String,
        i64,
        String,
        String,
        String,
        i64,
        i64,
        i64,
        i64,
        Option<String>,
        String,
        String,
        Option<i64>,
        bool,
        i64,
    )> = db(st
        .query_map(params![run.as_str(), after, limit], |r| {
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
        })
        .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>()))?;
    rows.into_iter()
        .map(|r| {
            Ok(StoredJobUsage {
                run_id: id_of(r.0)?,
                seq: r.1,
                request_id: id_of(r.2)?,
                purpose: r.3,
                model_id: id_of(r.4)?,
                input_tokens: r.5,
                output_tokens: r.6,
                cache_read_tokens: r.7,
                cache_write_tokens: r.8,
                amount_usd: r.9,
                price_kind: r.10,
                price_origin: r.11,
                price_revision: r.12,
                incomplete: r.13,
                recorded_at_ms: r.14,
            })
        })
        .collect()
}
/// A canonical decimal USD amount (at most 12 decimals) in pico-USD.
pub(crate) fn pico(amount: &str) -> Option<i128> {
    let (whole, frac) = amount.split_once('.').unwrap_or((amount, ""));
    if whole.is_empty()
        || frac.len() > 12
        || !whole
            .bytes()
            .chain(frac.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let whole: i128 = whole.parse().ok()?;
    let mut frac = frac.to_owned();
    while frac.len() < 12 {
        frac.push('0');
    }
    let frac: i128 = frac.parse().ok()?;
    whole.checked_mul(1_000_000_000_000)?.checked_add(frac)
}
fn usage_totals(conn: &Connection, run: &Id) -> Result<JobUsageTotals, StorageError> {
    let (items, untracked, incomplete): (i64, i64, i64) = db(conn.query_row(
        "SELECT count(*), ifnull(sum(price_kind='Untracked'),0), ifnull(sum(incomplete),0) FROM job_run_usage WHERE run_id=?1",
        params![run.as_str()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ))?;
    let mut st = db(conn.prepare_cached(
        "SELECT amount_usd FROM job_run_usage WHERE run_id=?1 AND amount_usd IS NOT NULL",
    ))?;
    let amounts = db(st
        .query_map(params![run.as_str()], |r| r.get::<_, String>(0))
        .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>()))?;
    let mut tracked: i128 = 0;
    for a in amounts {
        tracked = tracked
            .checked_add(pico(&a).ok_or_else(|| corrupt("stored usage amount"))?)
            .ok_or_else(|| corrupt("usage total overflow"))?;
    }
    Ok(JobUsageTotals {
        items,
        tracked_pico_usd: tracked,
        untracked,
        incomplete,
    })
}

macro_rules! job_reads {
    ($t:ty) => {
        impl JobRead for $t {
            fn job(&self, id: &Id) -> Result<Option<StoredJob>, StorageError> {
                job(self.conn, id)
            }
            fn jobs(
                &self,
                scope: &JobScope,
                enabled: Option<bool>,
                after: Option<(&str, &str)>,
                limit: u16,
            ) -> Result<Vec<StoredJob>, StorageError> {
                let limit = limit_ok(limit)?;
                let (scope_kind, project) = match scope {
                    JobScope::App => (0, None),
                    JobScope::Project(p) => (1, Some(p.0.as_str())),
                    JobScope::All => (2, None),
                };
                let (after_title, after_id, has_after) = match after {
                    Some((t, i)) => (t, i, 1),
                    None => ("", "", 0),
                };
                let enabled_filter: i64 = match enabled {
                    None => -1,
                    Some(true) => 1,
                    Some(false) => 0,
                };
                jobs_query(
                    self.conn,
                    &format!(
                        "SELECT {JOB_COLUMNS} FROM schedules WHERE deleted_at IS NULL \
                         AND (?1=2 OR (?1=0 AND project_id IS NULL) OR (?1=1 AND project_id=?2)) \
                         AND (?3=-1 OR enabled=?3) \
                         AND (?4=0 OR (title COLLATE NOCASE, id) > (?5 COLLATE NOCASE, ?6)) \
                         ORDER BY title COLLATE NOCASE, id LIMIT ?7"
                    ),
                    &[&scope_kind, &project, &enabled_filter, &has_after, &after_title, &after_id, &limit],
                )
            }
            fn job_titled(
                &self,
                project: Option<&ProjectId>,
                title: &str,
            ) -> Result<Option<StoredJob>, StorageError> {
                Ok(jobs_query(
                    self.conn,
                    &format!(
                        "SELECT {JOB_COLUMNS} FROM schedules WHERE ifnull(project_id,'')=ifnull(?1,'') \
                         AND title=?2 COLLATE NOCASE AND deleted_at IS NULL"
                    ),
                    &[&project.map(|p| p.0.as_str()), &title],
                )?
                .pop())
            }
            fn due_jobs(&self, now_ms: i64, limit: u16) -> Result<Vec<StoredJob>, StorageError> {
                let limit = limit_ok(limit)?;
                jobs_query(
                    self.conn,
                    &format!(
                        "SELECT {JOB_COLUMNS} FROM schedules WHERE enabled=1 AND next_at IS NOT NULL \
                         AND next_at<=?1 AND hold IS NULL AND deleted_at IS NULL ORDER BY next_at, id LIMIT ?2"
                    ),
                    &[&now_ms, &limit],
                )
            }
            fn all_jobs(&self, after: Option<&Id>, limit: u16) -> Result<Vec<StoredJob>, StorageError> {
                let limit = limit_ok(limit)?;
                jobs_query(
                    self.conn,
                    &format!(
                        "SELECT {JOB_COLUMNS} FROM schedules WHERE deleted_at IS NULL AND id>?1 ORDER BY id LIMIT ?2"
                    ),
                    &[&after.map_or("", Id::as_str), &limit],
                )
            }
            fn job_run(&self, id: &Id) -> Result<Option<StoredJobRun>, StorageError> {
                run(self.conn, id)
            }
            fn job_runs(
                &self,
                job: &Id,
                before_seq: Option<i64>,
                limit: u16,
            ) -> Result<Vec<StoredJobRun>, StorageError> {
                let limit = limit_ok(limit)?;
                runs_query(
                    self.conn,
                    &format!(
                        "SELECT {RUN_COLUMNS} FROM job_runs WHERE schedule_id=?1 AND seq<?2 ORDER BY seq DESC LIMIT ?3"
                    ),
                    &[&job.as_str(), &before_seq.unwrap_or(i64::MAX), &limit],
                )
            }
            fn active_run(&self, job: &Id) -> Result<Option<StoredJobRun>, StorageError> {
                active_run(self.conn, job)
            }
            fn runs_where(
                &self,
                filter: &RunFilter,
                after: Option<&Id>,
                limit: u16,
            ) -> Result<Vec<StoredJobRun>, StorageError> {
                let limit = limit_ok(limit)?;
                let after = after.map_or("", Id::as_str);
                let active = "state IN ('Queued','Running','Holding')";
                match filter {
                    RunFilter::Active => runs_query(
                        self.conn,
                        &format!("SELECT {RUN_COLUMNS} FROM job_runs WHERE {active} AND id>?1 ORDER BY id LIMIT ?2"),
                        &[&after, &limit],
                    ),
                    RunFilter::ActiveNotOfBoot(boot) => runs_query(
                        self.conn,
                        &format!(
                            "SELECT {RUN_COLUMNS} FROM job_runs WHERE {active} AND (boot_id IS NULL OR boot_id<>?3) \
                             AND id>?1 ORDER BY id LIMIT ?2"
                        ),
                        &[&after, &limit, &boot.as_str()],
                    ),
                    RunFilter::CancelRequested => runs_query(
                        self.conn,
                        &format!(
                            "SELECT {RUN_COLUMNS} FROM job_runs WHERE {active} AND cancel_requested=1 AND id>?1 ORDER BY id LIMIT ?2"
                        ),
                        &[&after, &limit],
                    ),
                }
            }
            fn run_links(&self, run: &Id) -> Result<Vec<RootId>, StorageError> {
                let mut st = db(self
                    .conn
                    .prepare_cached("SELECT root_id FROM job_run_links WHERE run_id=?1 ORDER BY root_id"))?;
                let rows = db(st
                    .query_map(params![run.as_str()], |r| r.get::<_, String>(0))
                    .and_then(|r| r.collect::<rusqlite::Result<Vec<_>>>()))?;
                rows.into_iter().map(|s| id_of(s).map(RootId)).collect()
            }
            fn checkpoint(&self, job: &Id, source: &str) -> Result<Option<StoredJobCheckpoint>, StorageError> {
                checkpoint(self.conn, job, source)
            }
            fn action(&self, job: &Id, key: &str) -> Result<Option<StoredJobAction>, StorageError> {
                action(self.conn, job, key)
            }
            fn unsettled_actions(&self, run: &Id) -> Result<Vec<StoredJobAction>, StorageError> {
                actions_query(
                    self.conn,
                    &format!("SELECT {ACTION_COLUMNS} FROM job_actions WHERE run_id=?1 AND state='Intent' ORDER BY action_key"),
                    &[&run.as_str()],
                )
            }
            fn run_usage(
                &self,
                run: &Id,
                after_seq: Option<i64>,
                limit: u16,
            ) -> Result<Vec<StoredJobUsage>, StorageError> {
                usage_rows(self.conn, run, after_seq.unwrap_or(0), limit_ok(limit)?)
            }
            fn usage_totals(&self, run: &Id) -> Result<JobUsageTotals, StorageError> {
                usage_totals(self.conn, run)
            }
        }
    };
}
job_reads!(Snapshot<'_>);
job_reads!(Unit<'_>);

impl JobRepository for Unit<'_> {
    fn insert_job(&mut self, j: &StoredJob) -> Result<StoredJob, StorageError> {
        validate_job(j)?;
        if job(self.conn, &j.id)?.is_some() {
            return Err(duplicate("job", j.id.as_str()));
        }
        if title_taken(self.conn, j)? {
            return Err(duplicate("job_title", j.title.clone()));
        }
        write_job(self.conn, j, 1)?;
        job(self.conn, &j.id)?.ok_or_else(|| corrupt("job vanished inside its transaction"))
    }
    fn update_job(
        &mut self,
        j: &StoredJob,
        expected_revision: i64,
    ) -> Result<StoredJob, StorageError> {
        validate_job(j)?;
        let current = job(self.conn, &j.id)?.ok_or_else(|| StorageError::NotFound {
            kind: "job",
            id: j.id.as_str().to_owned(),
        })?;
        if current.revision != expected_revision {
            return Err(conflict("job", &j.id, expected_revision, current.revision));
        }
        if current.project_id != j.project_id {
            return Err(invalid("a job never changes scope"));
        }
        if title_taken(self.conn, j)? {
            return Err(duplicate("job_title", j.title.clone()));
        }
        write_job(self.conn, j, expected_revision + 1)?;
        job(self.conn, &j.id)?.ok_or_else(|| corrupt("job vanished inside its transaction"))
    }
    fn set_job_schedule(
        &mut self,
        id: &Id,
        expected_revision: i64,
        next_at_ms: Option<i64>,
        interrupted_run_id: Option<&Id>,
    ) -> Result<StoredJob, StorageError> {
        let mut j = job(self.conn, id)?.ok_or_else(|| StorageError::NotFound {
            kind: "job",
            id: id.as_str().to_owned(),
        })?;
        if j.revision != expected_revision {
            return Err(conflict("job", id, expected_revision, j.revision));
        }
        j.next_at_ms = next_at_ms;
        j.interrupted_run_id = interrupted_run_id.cloned();
        validate_job(&j)?;
        if next_at_ms.is_some() && j.hold.is_some() {
            return Err(invalid("a held job schedules nothing"));
        }
        db(self.conn.execute(
            "UPDATE schedules SET next_at=?2, interrupted_run_id=?3 WHERE id=?1 AND revision=?4",
            params![
                id.as_str(),
                next_at_ms,
                interrupted_run_id.map(Id::as_str),
                expected_revision
            ],
        ))?;
        job(self.conn, id)?.ok_or_else(|| corrupt("job vanished inside its transaction"))
    }
    fn insert_run(&mut self, r: &StoredJobRun) -> Result<StoredJobRun, StorageError> {
        validate_run(r)?;
        if run(self.conn, &r.id)?.is_some() {
            return Err(duplicate("job_run", r.id.as_str()));
        }
        let taken: bool = db(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM job_runs WHERE schedule_id=?1 AND occurrence_key=?2)",
            params![r.job_id.as_str(), r.occurrence_key],
            |row| row.get(0),
        ))?;
        if taken {
            return Err(duplicate("job_occurrence", r.occurrence_key.clone()));
        }
        if ACTIVE_RUN_STATUSES.contains(&r.status.as_str())
            && active_run(self.conn, &r.job_id)?.is_some()
        {
            return Err(duplicate("active_run", r.job_id.as_str()));
        }
        let seq: i64 = db(self.conn.query_row(
            "SELECT ifnull(max(seq),0)+1 FROM job_runs WHERE schedule_id=?1",
            params![r.job_id.as_str()],
            |row| row.get(0),
        ))?;
        write_run(self.conn, r, seq, 1)?;
        run(self.conn, &r.id)?.ok_or_else(|| corrupt("run vanished inside its transaction"))
    }
    fn update_run(
        &mut self,
        r: &StoredJobRun,
        expected_revision: i64,
    ) -> Result<StoredJobRun, StorageError> {
        validate_run(r)?;
        let current = run(self.conn, &r.id)?.ok_or_else(|| StorageError::NotFound {
            kind: "job_run",
            id: r.id.as_str().to_owned(),
        })?;
        if current.revision != expected_revision {
            return Err(conflict(
                "job_run",
                &r.id,
                expected_revision,
                current.revision,
            ));
        }
        if current.job_id != r.job_id
            || current.occurrence_key != r.occurrence_key
            || current.definition != r.definition
            || current.trigger != r.trigger
        {
            return Err(invalid(
                "a run keeps its job, occurrence, trigger and definition",
            ));
        }
        let was_active = ACTIVE_RUN_STATUSES.contains(&current.status.as_str());
        if !was_active && ACTIVE_RUN_STATUSES.contains(&r.status.as_str()) {
            return Err(invalid("a finished run never becomes active again"));
        }
        if current.operation_id.is_some() && current.operation_id != r.operation_id {
            return Err(invalid("a run keeps its operation"));
        }
        write_run(self.conn, r, current.seq, expected_revision + 1)?;
        run(self.conn, &r.id)?.ok_or_else(|| corrupt("run vanished inside its transaction"))
    }
    fn set_run_checkpoint(&mut self, run_id: &Id, checkpoint: &str) -> Result<(), StorageError> {
        json_ok(checkpoint, "run checkpoint")?;
        let n = db(self.conn.execute(
            "UPDATE job_runs SET checkpoint_json=?2 WHERE id=?1 AND state IN ('Queued','Running','Holding')",
            params![run_id.as_str(), checkpoint],
        ))?;
        if n == 0 {
            return Err(StorageError::NotFound {
                kind: "active_job_run",
                id: run_id.as_str().to_owned(),
            });
        }
        Ok(())
    }
    fn link_root(&mut self, run_id: &Id, root: &RootId) -> Result<(), StorageError> {
        if run(self.conn, run_id)?.is_none() {
            return Err(StorageError::NotFound {
                kind: "job_run",
                id: run_id.as_str().to_owned(),
            });
        }
        db(self.conn.execute(
            "INSERT OR IGNORE INTO job_run_links(run_id,root_id) VALUES(?1,?2)",
            params![run_id.as_str(), root.0.as_str()],
        ))?;
        Ok(())
    }
    fn put_checkpoint(
        &mut self,
        c: &StoredJobCheckpoint,
        expected_revision: Option<i64>,
    ) -> Result<StoredJobCheckpoint, StorageError> {
        if c.source.is_empty() || c.source.len() > 256 {
            return Err(invalid("a checkpoint source is 1-256 bytes"));
        }
        json_ok(&c.cursor, "checkpoint cursor")?;
        let current = checkpoint(self.conn, &c.job_id, &c.source)?;
        let next = match (expected_revision, &current) {
            (None, None) => 1,
            (None, Some(_)) => return Err(duplicate("job_checkpoint", c.source.clone())),
            (Some(_), None) => {
                return Err(StorageError::NotFound {
                    kind: "job_checkpoint",
                    id: c.source.clone(),
                });
            }
            (Some(e), Some(cur)) if e != cur.revision => {
                return Err(conflict("job_checkpoint", &c.job_id, e, cur.revision));
            }
            (Some(e), Some(_)) => e + 1,
        };
        db(self.conn.execute(
            "INSERT INTO job_checkpoints(schedule_id,source,cursor_json,revision,run_id,updated_at) VALUES(?1,?2,?3,?4,?5,?6) \
             ON CONFLICT(schedule_id,source) DO UPDATE SET cursor_json=excluded.cursor_json, revision=excluded.revision, \
             run_id=excluded.run_id, updated_at=excluded.updated_at",
            params![c.job_id.as_str(), c.source, c.cursor, next, c.run_id.as_str(), c.updated_at_ms],
        ))?;
        checkpoint(self.conn, &c.job_id, &c.source)?
            .ok_or_else(|| corrupt("checkpoint vanished inside its transaction"))
    }
    fn insert_action(&mut self, a: &StoredJobAction) -> Result<StoredJobAction, StorageError> {
        validate_action(a)?;
        if action(self.conn, &a.job_id, &a.key)?.is_some() {
            return Err(duplicate("job_action", a.key.clone()));
        }
        db(self.conn.execute(
            "INSERT INTO job_actions(schedule_id,action_key,run_id,state,effect_json,revision,created_at,settled_at) VALUES(?1,?2,?3,?4,?5,1,?6,?7)",
            params![a.job_id.as_str(), a.key, a.run_id.as_str(), a.state, a.effect, a.created_at_ms, a.settled_at_ms],
        ))?;
        action(self.conn, &a.job_id, &a.key)?
            .ok_or_else(|| corrupt("action vanished inside its transaction"))
    }
    fn update_action(
        &mut self,
        a: &StoredJobAction,
        expected_revision: i64,
    ) -> Result<StoredJobAction, StorageError> {
        validate_action(a)?;
        let current =
            action(self.conn, &a.job_id, &a.key)?.ok_or_else(|| StorageError::NotFound {
                kind: "job_action",
                id: a.key.clone(),
            })?;
        if current.revision != expected_revision {
            return Err(conflict(
                "job_action",
                &a.job_id,
                expected_revision,
                current.revision,
            ));
        }
        if current.state == "Done" && a.state != "Done" {
            return Err(invalid("a done action stays done"));
        }
        db(self.conn.execute(
            "UPDATE job_actions SET run_id=?3, state=?4, effect_json=?5, revision=?6, settled_at=?7 WHERE schedule_id=?1 AND action_key=?2",
            params![a.job_id.as_str(), a.key, a.run_id.as_str(), a.state, a.effect, expected_revision + 1, a.settled_at_ms],
        ))?;
        action(self.conn, &a.job_id, &a.key)?
            .ok_or_else(|| corrupt("action vanished inside its transaction"))
    }
    fn record_usage(&mut self, u: &StoredJobUsage) -> Result<StoredJobUsage, StorageError> {
        if run(self.conn, &u.run_id)?.is_none() {
            return Err(StorageError::NotFound {
                kind: "job_run",
                id: u.run_id.as_str().to_owned(),
            });
        }
        if (u.price_kind == "Untracked") != u.amount_usd.is_none() {
            return Err(invalid(
                "untracked usage has no amount; tracked usage has one",
            ));
        }
        if u.amount_usd.as_deref().is_some_and(|a| pico(a).is_none()) {
            return Err(invalid("usage amount must be a canonical decimal"));
        }
        let taken: bool = db(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM job_run_usage WHERE run_id=?1 AND request_id=?2)",
            params![u.run_id.as_str(), u.request_id.as_str()],
            |r| r.get(0),
        ))?;
        if taken {
            return Err(duplicate("job_usage", u.request_id.as_str()));
        }
        let seq: i64 = db(self.conn.query_row(
            "SELECT ifnull(max(seq),0)+1 FROM job_run_usage WHERE run_id=?1",
            params![u.run_id.as_str()],
            |r| r.get(0),
        ))?;
        db(self.conn.execute(
            &format!("INSERT INTO job_run_usage({USAGE_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"),
            params![
                u.run_id.as_str(),
                seq,
                u.request_id.as_str(),
                u.purpose,
                u.model_id.as_str(),
                u.input_tokens,
                u.output_tokens,
                u.cache_read_tokens,
                u.cache_write_tokens,
                u.amount_usd,
                u.price_kind,
                u.price_origin,
                u.price_revision,
                u.incomplete,
                u.recorded_at_ms,
            ],
        ))?;
        usage_rows(self.conn, &u.run_id, seq - 1, 1)?
            .pop()
            .ok_or_else(|| corrupt("usage vanished inside its transaction"))
    }
}
