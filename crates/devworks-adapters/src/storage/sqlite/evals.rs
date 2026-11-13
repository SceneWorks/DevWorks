//! Bounded, insert-only eval reports (migration 24).
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{Id, ProjectId};
use rusqlite::{Connection, OptionalExtension, params};

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn corrupt(message: impl Into<String>) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, message)
}
fn id(value: String) -> Result<Id, StorageError> {
    Id::new(value).map_err(|_| corrupt("invalid eval identifier"))
}
fn runs<P: rusqlite::Params>(
    conn: &Connection,
    tail: &str,
    params: P,
) -> Result<Vec<StoredEvalRun>, StorageError> {
    let mut statement = db(conn.prepare_cached(&format!(
        "SELECT id,suite_id,suite_revision,project_id,session_id,session_revision,session_sequence,measurement_digest,created_at,source_watermark,report_json FROM eval_runs WHERE {tail}"
    )))?;
    let rows = db(db(statement.query_map(params, |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, i64>(9)?,
            row.get::<_, String>(10)?,
        ))
    }))?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    rows.into_iter()
        .map(
            |(
                run,
                suite,
                suite_revision,
                project,
                session,
                session_revision,
                session_sequence,
                measurement_digest,
                created_at_ms,
                source_watermark,
                report,
            )| {
                Ok(StoredEvalRun {
                    id: id(run)?,
                    suite_id: id(suite)?,
                    suite_revision,
                    project_id: project.map(id).transpose()?.map(ProjectId),
                    session_id: id(session)?,
                    session_revision,
                    session_sequence,
                    measurement_digest,
                    created_at_ms,
                    source_watermark,
                    report: serde_json::from_str(&report)
                        .map_err(|_| corrupt("invalid eval report JSON"))?,
                })
            },
        )
        .collect()
}
macro_rules! eval_reads {
    ($type:ty) => {
        impl EvalRead for $type {
            fn eval_run(&self, id: &Id) -> Result<Option<StoredEvalRun>, StorageError> {
                Ok(runs(self.conn, "id=?1", [id.as_str()])?.into_iter().next())
            }
            fn eval_runs(
                &self,
                session_id: &Id,
                after: Option<&Id>,
                limit: u16,
            ) -> Result<Vec<StoredEvalRun>, StorageError> {
                if limit == 0 || limit > MAX_PAGE {
                    return Err(invalid("eval page limit out of bounds"));
                }
                runs(
                    self.conn,
                    "session_id=?1 AND id>?2 ORDER BY id LIMIT ?3",
                    params![
                        session_id.as_str(),
                        after.map_or("", Id::as_str),
                        i64::from(limit)
                    ],
                )
            }
        }
    };
}
eval_reads!(Snapshot<'_>);
eval_reads!(Unit<'_>);

impl EvalRepository for Unit<'_> {
    fn insert_eval_run(&mut self, run: &StoredEvalRun) -> Result<(), StorageError> {
        if run.suite_revision < 0
            || run.session_revision < 0
            || run.session_sequence < 0
            || run.created_at_ms < 0
            || run.source_watermark < 0
        {
            return Err(invalid(
                "eval revisions, sequence, time and watermark must be nonnegative",
            ));
        }
        if !devworks_core::fingerprint::is_sha256_hex(&run.measurement_digest) {
            return Err(invalid("eval measurement digest must be lowercase SHA-256"));
        }
        let report = serde_json::to_string(&run.report)
            .map_err(|_| invalid("eval report cannot be serialized"))?;
        if report.len() > MAX_EVAL_REPORT_BYTES {
            return Err(invalid("eval report exceeds size bound"));
        }
        if self.eval_run(&run.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "eval_run",
                id: run.id.as_str().into(),
            });
        }
        let session = db(self
            .conn
            .query_row(
                "SELECT project_id,revision,next_seq FROM sessions WHERE id=?1",
                [run.session_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional())?
        .ok_or_else(|| StorageError::NotFound {
            kind: "session",
            id: run.session_id.as_str().into(),
        })?;
        if session.0.as_deref() != run.project_id.as_ref().map(|project| project.0.as_str()) {
            return Err(StorageError::ScopeDenied);
        }
        if session.1 != run.session_revision {
            return Err(invalid("eval source session revision changed"));
        }
        if session.2 - 1 != run.session_sequence {
            return Err(invalid("eval source transcript sequence changed"));
        }
        if run.source_watermark > self.committed_sequence()? {
            return Err(invalid("eval source watermark is in the future"));
        }
        db(self.conn.execute(
            "INSERT INTO eval_runs(id,suite_id,suite_revision,project_id,session_id,session_revision,session_sequence,measurement_digest,created_at,source_watermark,report_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![run.id.as_str(), run.suite_id.as_str(), run.suite_revision, run.project_id.as_ref().map(|project| project.0.as_str()), run.session_id.as_str(), run.session_revision, run.session_sequence, run.measurement_digest, run.created_at_ms, run.source_watermark, report],
        ))?;
        Ok(())
    }
}
