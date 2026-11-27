//! Immutable analysis inputs and short, revision-fenced phase updates.
use super::{
    conn::db,
    repos::{Snapshot, Unit},
    support::{conflict, corrupt, invalid, page_limit},
};
use devworks_core::context::PrincipalKind;
use devworks_core::fingerprint::is_sha256_hex;
use devworks_core::ports::storage::*;
use devworks_core::protocol::{Id, RevisionDimension, data::DataScope};
use rusqlite::{Connection, OptionalExtension, params};

fn decode(text: &str) -> Result<StoredAnalysisRun, StorageError> {
    serde_json::from_str(text).map_err(|_| corrupt("invalid analysis run"))
}

fn encode(run: &StoredAnalysisRun) -> Result<String, StorageError> {
    let views = &run.intent.views;
    if !is_sha256_hex(&run.binding_key)
        || !is_sha256_hex(&run.lineage_key)
        || !is_sha256_hex(&run.intent.options_digest)
        || !is_sha256_hex(&run.owner.namespace)
        || run.revision < 1
        || run.created_at_ms > run.updated_at_ms
        || run.intent.configuration_revision < 1
        || views.is_empty()
        || views.len() != run.intent.repository_lineages.len()
        || !run
            .intent
            .repository_lineages
            .iter()
            .all(|value| is_sha256_hex(value))
        || views.len() + run.intent.unavailable.len() > 32
        || run.intent.batch_limit == 0
        || run.intent.batch_limit > 500
        || run.intent.witnesses.len() != views.len()
        || run
            .intent
            .witnesses
            .windows(2)
            .any(|w| w[0].view_id.as_str() >= w[1].view_id.as_str())
        || views
            .windows(2)
            .any(|pair| pair[0].repository_id.as_str() >= pair[1].repository_id.as_str())
        || views.iter().any(|view| {
            view.repository_id != view.snapshot.repository_id
                || view.snapshot.validate().is_err()
                || !run
                    .intent
                    .witnesses
                    .iter()
                    .any(|witness| witness.view_id == view.view_id && witness.source_revision > 0)
        })
        || run.phases.is_empty()
        || run.phases.len() > 7
        || run.phases.iter().enumerate().any(|(index, phase)| {
            run.phases[..index]
                .iter()
                .any(|earlier| earlier.phase == phase.phase)
        })
        || run.phases.iter().any(|phase| {
            phase.subjects.len() > 32
                || phase.subjects.iter().any(|subject| {
                    !is_sha256_hex(&subject.prompt_digest)
                        || !views.iter().any(|view| {
                            view.repository_id == subject.repository && view.view_id == subject.view
                        })
                })
        })
        || run.intent.node_ids.len() > 128
        || PrincipalKind::parse(&run.owner.kind).is_none()
    {
        return Err(invalid("invalid analysis run binding"));
    }
    serde_json::to_string(run).map_err(|_| invalid("analysis run cannot be encoded"))
}

fn one(conn: &Connection, id: &Id) -> Result<Option<StoredAnalysisRun>, StorageError> {
    db(conn
        .query_row(
            "SELECT payload_json FROM analysis_runs WHERE id=?1",
            [id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional())?
    .map(|row| decode(&row))
    .transpose()
}

fn list(
    conn: &Connection,
    digest: &str,
    lineage: bool,
    after: Option<&AnalysisRunPosition>,
    limit: u16,
) -> Result<Vec<StoredAnalysisRun>, StorageError> {
    let limit = page_limit(limit)?;
    let column = if lineage {
        "lineage_key"
    } else {
        "binding_key"
    };
    let mut query = db(conn.prepare(&format!(
        "SELECT payload_json FROM analysis_runs WHERE {column}=?1 \
         AND (?2 IS NULL OR requested_at_ms < ?2 OR (requested_at_ms = ?2 AND id > ?3)) \
         ORDER BY requested_at_ms DESC, id LIMIT ?4"
    )))?;
    db(query.query_map(
        params![
            digest,
            after.map(|pos| pos.created_at_ms),
            after.map(|pos| pos.id.as_str()),
            limit
        ],
        |row| row.get::<_, String>(0),
    ))?
    .map(|row| db(row).and_then(|row| decode(&row)))
    .collect()
}

macro_rules! analysis_reads {
    ($reader:ty) => {
        impl AnalysisRead for $reader {
            fn analysis_retention_page(
                &self, after: Option<&AnalysisRunPosition>, limit: u16,
            ) -> Result<Vec<StoredAnalysisRun>, StorageError> {
                let limit = page_limit(limit)?;
                let mut query = db(self.conn.prepare(
                    "SELECT payload_json FROM analysis_runs
                     WHERE (?1 IS NULL OR requested_at_ms < ?1 OR (requested_at_ms = ?1 AND id > ?2))
                     ORDER BY requested_at_ms DESC, id LIMIT ?3",
                ))?;
                db(query.query_map(
                    params![after.map(|pos| pos.created_at_ms), after.map(|pos| pos.id.as_str()), limit],
                    |row| row.get::<_, String>(0),
                ))?.map(|row| db(row).and_then(|row| decode(&row))).collect()
            }

            fn analysis_active_reference(&self, scope: &str, run: &Id) -> Result<bool, StorageError> {
                db(self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM analysis_runs WHERE scope_key=?1
                       AND json_extract(payload_json, '$.intent.prerequisite_run')=?2
                       AND json_extract(payload_json, '$.state')
                           NOT IN ('ObservedComplete','Failed','CancelledBeforeDispatch'))",
                    params![scope, run.as_str()], |row| row.get(0),
                ))
            }

            fn active_model_runs(&self, scope_key: &str) -> Result<usize, StorageError> {
                db(self.conn.query_row(
                    "SELECT COUNT(*) FROM (
                        SELECT 1 FROM analysis_runs AS run
                        WHERE run.scope_key=?1
                          AND json_extract(run.payload_json,'$.state')
                            NOT IN ('ObservedComplete','Failed','CancelledBeforeDispatch')
                          AND EXISTS (
                            SELECT 1 FROM json_each(run.payload_json,'$.phases') AS phase
                            WHERE json_extract(phase.value,'$.phase') IN ('Projects','Synthesis')
                              AND json_extract(phase.value,'$.state') != 'ObservedComplete'
                          )
                        LIMIT 32
                     )",
                    [scope_key],
                    |row| row.get::<_, u32>(0),
                )).map(|count| count as usize)
            }

            fn analysis_repository_runs(
                &self, scope: Option<&str>, repositories: &[Id], oldest_ms: i64, limit: u16,
            ) -> Result<Vec<StoredAnalysisRun>, StorageError> {
                let limit = page_limit(limit)?;
                let repositories = serde_json::to_string(repositories)
                    .map_err(|_| invalid("invalid analysis repository filter"))?;
                let mut query = db(self.conn.prepare(
                    "SELECT payload_json FROM analysis_runs AS run
                     WHERE (?1 IS NULL OR run.scope_key=?1) AND run.requested_at_ms >= ?2
                     AND EXISTS (
                       SELECT 1 FROM json_each(run.payload_json,'$.phases') AS phase
                       WHERE json_extract(phase.value,'$.phase') IN ('Vitals','Security')
                         AND json_extract(phase.value,'$.state')='ObservedComplete')
                     AND EXISTS (
                       SELECT 1 FROM json_each(run.payload_json,'$.intent.views') AS view
                       JOIN json_each(?3) AS selected
                         ON selected.value=json_extract(view.value,'$.repositoryId'))
                     ORDER BY requested_at_ms DESC, id LIMIT ?4",
                ))?;
                db(query.query_map(
                    params![scope, oldest_ms, repositories, limit],
                    |row| row.get::<_, String>(0),
                ))?.map(|row| db(row).and_then(|row| decode(&row))).collect()
            }

            fn analysis_run(&self, id: &Id) -> Result<Option<StoredAnalysisRun>, StorageError> {
                one(self.conn, id)
            }

            fn analysis_runs(
                &self,
                digest: &str,
                lineage: bool,
                after: Option<&AnalysisRunPosition>,
                limit: u16,
            ) -> Result<Vec<StoredAnalysisRun>, StorageError> {
                list(self.conn, digest, lineage, after, limit)
            }
        }
    };
}
analysis_reads!(Snapshot<'_>);
analysis_reads!(Unit<'_>);

impl AnalysisRepository for Unit<'_> {
    fn remove_terminal_analysis_run(
        &mut self,
        id: &Id,
        expected_revision: i64,
    ) -> Result<(), StorageError> {
        let current = self.analysis_run(id)?.ok_or(StorageError::Busy)?;
        if !current.terminal() || current.revision != expected_revision {
            return Err(StorageError::Busy);
        }
        db(self.conn.execute(
            "DELETE FROM analysis_runs WHERE id=?1 AND revision=?2",
            params![id.as_str(), expected_revision],
        ))?;
        Ok(())
    }

    fn insert_analysis_run(&mut self, run: &StoredAnalysisRun) -> Result<(), StorageError> {
        if run.revision != 1 || run.operation != run.active_operation {
            return Err(invalid("a new analysis run starts at revision one"));
        }
        let scope = match &run.intent.scope {
            DataScope::Workspace => WORKSPACE_SCOPE_KEY,
            DataScope::Project { project_id } => project_id.0.as_str(),
        };
        db(self.conn.execute(
            "INSERT INTO analysis_runs(id, operation_id, scope_key, binding_key, lineage_key, \
             requested_at_ms, revision, payload_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                run.id.as_str(),
                run.operation.0.as_str(),
                scope,
                run.binding_key,
                run.lineage_key,
                run.created_at_ms,
                run.revision,
                encode(run)?,
            ],
        ))?;
        Ok(())
    }

    fn update_analysis_run(
        &mut self,
        run: &StoredAnalysisRun,
        expected_revision: i64,
    ) -> Result<(), StorageError> {
        let prior = self
            .analysis_run(&run.id)?
            .ok_or_else(|| invalid("analysis run is missing"))?;
        if expected_revision != prior.revision {
            return Err(conflict(
                RevisionDimension::Operation,
                "analysis_run",
                run.id.as_str(),
                expected_revision,
                prior.revision,
            ));
        }
        if run.revision != prior.revision + 1
            || (run.active_operation != prior.active_operation || run.pin_id != prior.pin_id)
                && !matches!(
                    prior.state,
                    devworks_core::protocol::OperationState::Failed
                        | devworks_core::protocol::OperationState::CancelledBeforeDispatch
                )
            || prior.operation != run.operation
            || prior.binding_key != run.binding_key
            || prior.lineage_key != run.lineage_key
            || prior.created_at_ms != run.created_at_ms
            || prior.updated_at_ms > run.updated_at_ms
            || serde_json::to_value(&prior.owner).ok() != serde_json::to_value(&run.owner).ok()
            || serde_json::to_value(&prior.intent).ok() != serde_json::to_value(&run.intent).ok()
        {
            return Err(invalid(
                "analysis updates cannot replace the original intent",
            ));
        }
        db(self.conn.execute(
            "UPDATE analysis_runs SET revision=?2, payload_json=?3 WHERE id=?1 AND revision=?4",
            params![
                run.id.as_str(),
                run.revision,
                encode(run)?,
                expected_revision
            ],
        ))?;
        Ok(())
    }
}
