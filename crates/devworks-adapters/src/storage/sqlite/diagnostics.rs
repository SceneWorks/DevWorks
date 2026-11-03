//! SQLite diagnostic log records: `diagnostic_records` (migration 12) and the storage health
//! probes of [`SqliteStore`] (OPS-01).
use super::conn::db;
use super::repos::{Snapshot, Unit};
use super::{DATABASE_FILE, MIGRATIONS, SqliteStore, migrations};
use devworks_core::ports::diagnostics::{
    Metric, StorageDatabase, StorageFiles, StorageHealthProbe,
};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, OperationId, ProjectId};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

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

const COLUMNS: &str = "sequence,id,recorded_at,severity,component,code,message,origin,actor_id,correlation_id,project_id,operation,operation_id,target_kind,target_id,stage,source_revision,attempt,started_at,finished_at,progress_completed,progress_total,outcome,error_class,output,output_omitted_bytes,redactions,evidence_json,metrics_json,recovery_json";

struct Raw {
    sequence: i64,
    id: String,
    recorded_at: i64,
    severity: String,
    component: String,
    code: String,
    message: String,
    origin: String,
    actor: String,
    correlation: Option<String>,
    project: Option<String>,
    operation: Option<String>,
    operation_id: Option<String>,
    target_kind: Option<String>,
    target_id: Option<String>,
    stage: Option<String>,
    source_revision: Option<String>,
    attempt: Option<i64>,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    completed: Option<i64>,
    total: Option<i64>,
    outcome: Option<String>,
    error_class: Option<String>,
    output: Option<String>,
    omitted: i64,
    redactions: i64,
    evidence: Option<String>,
    metrics: String,
    recovery: String,
}
fn raw(r: &Row<'_>) -> rusqlite::Result<Raw> {
    Ok(Raw {
        sequence: r.get(0)?,
        id: r.get(1)?,
        recorded_at: r.get(2)?,
        severity: r.get(3)?,
        component: r.get(4)?,
        code: r.get(5)?,
        message: r.get(6)?,
        origin: r.get(7)?,
        actor: r.get(8)?,
        correlation: r.get(9)?,
        project: r.get(10)?,
        operation: r.get(11)?,
        operation_id: r.get(12)?,
        target_kind: r.get(13)?,
        target_id: r.get(14)?,
        stage: r.get(15)?,
        source_revision: r.get(16)?,
        attempt: r.get(17)?,
        started_at: r.get(18)?,
        finished_at: r.get(19)?,
        completed: r.get(20)?,
        total: r.get(21)?,
        outcome: r.get(22)?,
        error_class: r.get(23)?,
        output: r.get(24)?,
        omitted: r.get(25)?,
        redactions: r.get(26)?,
        evidence: r.get(27)?,
        metrics: r.get(28)?,
        recovery: r.get(29)?,
    })
}
fn record(r: Raw) -> Result<StoredDiagnostic, StorageError> {
    Ok(StoredDiagnostic {
        sequence: r.sequence,
        id: id_of(r.id)?,
        recorded_at_ms: r.recorded_at,
        severity: r.severity,
        component: r.component,
        code: r.code,
        message: r.message,
        origin: r.origin,
        actor: ActorId(id_of(r.actor)?),
        correlation_id: opt_id(r.correlation)?,
        project_id: opt_id(r.project)?.map(ProjectId),
        operation: r.operation,
        operation_id: opt_id(r.operation_id)?.map(OperationId),
        target_kind: r.target_kind,
        target_id: opt_id(r.target_id)?,
        stage: r.stage,
        source_revision: r.source_revision,
        attempt: r.attempt,
        started_at_ms: r.started_at,
        finished_at_ms: r.finished_at,
        progress_completed: r.completed,
        progress_total: r.total,
        outcome: r.outcome,
        error_class: r.error_class,
        output: r.output,
        output_omitted_bytes: r.omitted,
        redactions: r.redactions,
        evidence_json: r.evidence,
        metrics_json: r.metrics,
        recovery_json: r.recovery,
    })
}

fn equals(column: &str, v: Option<String>, clauses: &mut Vec<String>, p: &mut Vec<Sql>) {
    if let Some(v) = v {
        clauses.push(format!("{column} = ?"));
        p.push(Sql::Text(v));
    }
}

/// The `WHERE` clause and parameters of a filter (without ordering or limit).
fn predicate(f: &DiagnosticFilter) -> (String, Vec<Sql>) {
    let mut clauses = vec!["sequence > ?".to_owned()];
    let mut p: Vec<Sql> = vec![Sql::Integer(f.after_sequence)];
    if let Some(b) = f.before_sequence {
        clauses.push("sequence < ?".into());
        p.push(Sql::Integer(b));
    }
    if let Some(c) = &f.component {
        // Exact, case-sensitive: `LIKE` folds ASCII case, which would let a component spelled in
        // another case pass for a host channel.
        clauses.push("(component = ? OR substr(component, 1, ?) = ?)".into());
        p.push(Sql::Text(c.clone()));
        p.push(Sql::Integer(c.chars().count() as i64 + 1));
        p.push(Sql::Text(format!("{c}.")));
    }
    if let Some(r) = f.min_severity {
        clauses.push("severity_rank >= ?".into());
        p.push(Sql::Integer(i64::from(r)));
    }
    let text = |i: Option<&Id>| i.map(|i| i.as_str().to_owned());
    equals("outcome", f.outcome.clone(), &mut clauses, &mut p);
    equals("operation", f.operation.clone(), &mut clauses, &mut p);
    equals(
        "operation_id",
        text(f.operation_id.as_ref().map(|o| &o.0)),
        &mut clauses,
        &mut p,
    );
    equals(
        "correlation_id",
        text(f.correlation_id.as_ref()),
        &mut clauses,
        &mut p,
    );
    equals(
        "target_id",
        text(f.target_id.as_ref()),
        &mut clauses,
        &mut p,
    );
    equals("stage", f.stage.clone(), &mut clauses, &mut p);
    equals(
        "project_id",
        text(f.project_id.as_ref().map(|o| &o.0)),
        &mut clauses,
        &mut p,
    );
    if let DiagnosticAudience::Project(project) = &f.audience {
        clauses.push("(project_id IS NULL OR project_id = ?)".into());
        p.push(Sql::Text(project.0.as_str().to_owned()));
    }
    if let Some(s) = &f.search {
        // ASCII case-insensitive substring of any searchable member.
        clauses.push(
            "(instr(lower(code), lower(?)) > 0 OR instr(lower(message), lower(?)) > 0 OR instr(lower(coalesce(stage, '')), lower(?)) > 0 OR instr(lower(coalesce(operation, '')), lower(?)) > 0 OR instr(lower(coalesce(output, '')), lower(?)) > 0)"
                .into(),
        );
        for _ in 0..5 {
            p.push(Sql::Text(s.clone()));
        }
    }
    if let Some(t) = f.since_ms {
        clauses.push("recorded_at >= ?".into());
        p.push(Sql::Integer(t));
    }
    if let Some(t) = f.until_ms {
        clauses.push("recorded_at <= ?".into());
        p.push(Sql::Integer(t));
    }
    (clauses.join(" AND "), p)
}

fn list(
    conn: &Connection,
    f: &DiagnosticFilter,
    limit: u16,
) -> Result<Vec<StoredDiagnostic>, StorageError> {
    if limit == 0 || limit > MAX_PAGE {
        return Err(invalid(format!("limit must be 1..={MAX_PAGE}")));
    }
    let (clause, mut p) = predicate(f);
    p.push(Sql::Integer(i64::from(limit)));
    let order = if f.descending { "DESC" } else { "ASC" };
    let mut s = db(conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM diagnostic_records WHERE {clause} ORDER BY sequence {order} LIMIT ?"
    )))?;
    let rows =
        db(db(s.query_map(params_from_iter(p), raw))?.collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter().map(record).collect()
}
fn count(conn: &Connection, f: &DiagnosticFilter) -> Result<i64, StorageError> {
    let (clause, p) = predicate(f);
    db(conn.query_row(
        &format!("SELECT count(*) FROM diagnostic_records WHERE {clause}"),
        params_from_iter(p),
        |r| r.get(0),
    ))
}
fn one(conn: &Connection, id: &Id) -> Result<Option<StoredDiagnostic>, StorageError> {
    let found = db(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM diagnostic_records WHERE id=?1"),
            [id.as_str()],
            raw,
        )
        .optional())?;
    found.map(record).transpose()
}
fn stats(conn: &Connection) -> Result<DiagnosticStats, StorageError> {
    let (records, oldest): (i64, Option<i64>) = db(conn.query_row(
        "SELECT count(*), min(recorded_at) FROM diagnostic_records",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ))?;
    let latest: Option<i64> = db(conn
        .query_row(
            "SELECT seq FROM sqlite_sequence WHERE name='diagnostic_records'",
            [],
            |r| r.get(0),
        )
        .optional())?;
    Ok(DiagnosticStats {
        records,
        latest_sequence: latest.unwrap_or(0),
        oldest_recorded_at_ms: oldest,
    })
}

macro_rules! diagnostic_reads {
    ($t:ty) => {
        impl DiagnosticRead for $t {
            fn diagnostic(&self, id: &Id) -> Result<Option<StoredDiagnostic>, StorageError> {
                one(self.conn, id)
            }
            fn diagnostics(
                &self,
                filter: &DiagnosticFilter,
                limit: u16,
            ) -> Result<Vec<StoredDiagnostic>, StorageError> {
                list(self.conn, filter, limit)
            }
            fn count_diagnostics(&self, filter: &DiagnosticFilter) -> Result<i64, StorageError> {
                count(self.conn, filter)
            }
            fn diagnostic_stats(&self) -> Result<DiagnosticStats, StorageError> {
                stats(self.conn)
            }
        }
    };
}
diagnostic_reads!(Snapshot<'_>);
diagnostic_reads!(Unit<'_>);

impl DiagnosticRepository for Unit<'_> {
    fn append_diagnostic(
        &mut self,
        d: &StoredDiagnostic,
    ) -> Result<StoredDiagnostic, StorageError> {
        let rank = severity_rank(&d.severity)
            .ok_or_else(|| invalid(format!("unknown severity {:?}", d.severity)))?;
        if one(self.conn, &d.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "diagnostic",
                id: d.id.as_str().to_owned(),
            });
        }
        let opt = |v: &Option<Id>| v.as_ref().map(|i| i.as_str().to_owned());
        db(self.conn.execute(
            "INSERT INTO diagnostic_records(id,recorded_at,severity,severity_rank,component,code,message,origin,actor_id,correlation_id,project_id,operation,operation_id,target_kind,target_id,stage,source_revision,attempt,started_at,finished_at,progress_completed,progress_total,outcome,error_class,output,output_omitted_bytes,redactions,evidence_json,metrics_json,recovery_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30)",
            params![
                d.id.as_str(),
                d.recorded_at_ms,
                d.severity,
                i64::from(rank),
                d.component,
                d.code,
                d.message,
                d.origin,
                d.actor.0.as_str(),
                opt(&d.correlation_id),
                d.project_id.as_ref().map(|p| p.0.as_str().to_owned()),
                d.operation,
                d.operation_id.as_ref().map(|o| o.0.as_str().to_owned()),
                d.target_kind,
                opt(&d.target_id),
                d.stage,
                d.source_revision,
                d.attempt,
                d.started_at_ms,
                d.finished_at_ms,
                d.progress_completed,
                d.progress_total,
                d.outcome,
                d.error_class,
                d.output,
                d.output_omitted_bytes,
                d.redactions,
                d.evidence_json,
                d.metrics_json,
                d.recovery_json,
            ],
        ))?;
        let sequence = self.conn.last_insert_rowid();
        Ok(StoredDiagnostic {
            sequence,
            ..d.clone()
        })
    }

    fn prune_diagnostics(
        &mut self,
        older_than_ms: i64,
        keep_at_most: i64,
    ) -> Result<PrunedDiagnostics, StorageError> {
        if keep_at_most < 0 {
            return Err(invalid("the record cap is non-negative"));
        }
        let expired = db(self.conn.execute(
            "DELETE FROM diagnostic_records WHERE recorded_at < ?1",
            [older_than_ms],
        ))? as i64;
        let over_cap = db(self.conn.execute(
            "DELETE FROM diagnostic_records WHERE sequence IN (SELECT sequence FROM diagnostic_records ORDER BY sequence DESC LIMIT -1 OFFSET ?1)",
            [keep_at_most],
        ))? as i64;
        Ok(PrunedDiagnostics { expired, over_cap })
    }
}

// ---------------------------------------------------------------------------------------------
// Storage health probes

fn file_bytes(path: &Path) -> Metric {
    match std::fs::metadata(path) {
        Ok(m) => Metric::Available(m.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Metric::Available(0),
        Err(e) => Metric::Unavailable(format!("{}: {e}", path.display())),
    }
}

/// Bytes available to this user and the size of the volume holding `dir`.
#[cfg(unix)]
// `statvfs` field widths differ between platforms (32-bit block counts on macOS).
#[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
fn volume(dir: &Path) -> (Metric, Metric) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        let why = "the data-root path contains a NUL byte".to_owned();
        return (Metric::Unavailable(why.clone()), Metric::Unavailable(why));
    };
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a NUL-terminated path and `s` a writable statvfs the call fills.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        let why = format!("statvfs failed: {}", std::io::Error::last_os_error());
        return (Metric::Unavailable(why.clone()), Metric::Unavailable(why));
    }
    let frsize = if s.f_frsize > 0 {
        s.f_frsize as u64
    } else {
        s.f_bsize as u64
    };
    (
        Metric::Available((s.f_bavail as u64).saturating_mul(frsize)),
        Metric::Available((s.f_blocks as u64).saturating_mul(frsize)),
    )
}
#[cfg(windows)]
fn volume(dir: &Path) -> (Metric, Metric) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut available, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: `wide` is a NUL-terminated UTF-16 path and the outputs are writable u64s.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free) };
    if ok == 0 {
        let why = format!(
            "GetDiskFreeSpaceExW failed: {}",
            std::io::Error::last_os_error()
        );
        return (Metric::Unavailable(why.clone()), Metric::Unavailable(why));
    }
    (Metric::Available(available), Metric::Available(total))
}
#[cfg(not(any(unix, windows)))]
fn volume(_: &Path) -> (Metric, Metric) {
    let why = "free-space reporting is not supported on this platform".to_owned();
    (Metric::Unavailable(why.clone()), Metric::Unavailable(why))
}

/// Schema object names (tables, indexes, triggers, views) of a connection, excluding SQLite's
/// own and FTS shadow tables' internals that SQLite creates itself.
fn objects(conn: &Connection) -> Result<BTreeSet<String>, StorageError> {
    let mut s = db(conn.prepare(
        "SELECT type || ':' || name FROM sqlite_schema WHERE name NOT LIKE 'sqlite_stat%'",
    ))?;
    let names = db(db(s.query_map([], |r| r.get::<_, String>(0)))?.collect())?;
    Ok(names)
}

/// The schema objects of a freshly migrated store at `version` (computed once per version).
fn expected_objects(version: i64) -> Result<BTreeSet<String>, StorageError> {
    static CACHE: Mutex<BTreeMap<i64, BTreeSet<String>>> = Mutex::new(BTreeMap::new());
    if let Some(found) = CACHE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&version)
    {
        return Ok(found.clone());
    }
    let conn = db(Connection::open_in_memory())?;
    db(conn.pragma_update(None, "foreign_keys", true))?;
    let through: Vec<_> = MIGRATIONS
        .iter()
        .copied()
        .filter(|m| m.version <= version)
        .collect();
    migrations::prepare(&conn, &through, || "expected-schema".into(), 0, "health")?;
    let found = objects(&conn)?;
    CACHE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(version, found.clone());
    Ok(found)
}

impl SqliteStore {
    fn directory(&self) -> &Path {
        self.database_path().parent().unwrap_or(Path::new("."))
    }
}

impl StorageHealthProbe for SqliteStore {
    fn files(&self) -> StorageFiles {
        let dir = self.directory();
        let (free, total) = volume(dir);
        StorageFiles {
            database_bytes: file_bytes(&dir.join(DATABASE_FILE)),
            wal_bytes: file_bytes(&dir.join(format!("{DATABASE_FILE}-wal"))),
            free_bytes: free,
            total_bytes: total,
        }
    }

    fn database(&self) -> Result<StorageDatabase, StorageError> {
        let compatibility = self.compatibility()?;
        let lease = self.lease(None)?;
        let conn = lease.conn.as_ref().expect("leased");
        let journal_mode: String = db(conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)))?;
        let page_size: i64 = db(conn.query_row("PRAGMA page_size", [], |r| r.get(0)))?;
        let page_count: i64 = db(conn.query_row("PRAGMA page_count", [], |r| r.get(0)))?;
        let freelist: i64 = db(conn.query_row("PRAGMA freelist_count", [], |r| r.get(0)))?;
        let present = objects(conn)?;
        let expected = expected_objects(compatibility.store_schema_version)?;
        let missing: Vec<String> = expected.difference(&present).cloned().collect();
        Ok(StorageDatabase {
            compatibility,
            journal_mode: journal_mode.to_ascii_lowercase(),
            page_size: page_size.max(0) as u64,
            page_count: page_count.max(0) as u64,
            freelist_pages: freelist.max(0) as u64,
            expected_objects: expected.len() as u64,
            missing_objects: missing,
        })
    }

    fn integrity(&self) -> Result<IntegrityReport, StorageError> {
        self.integrity_check()
    }
}
