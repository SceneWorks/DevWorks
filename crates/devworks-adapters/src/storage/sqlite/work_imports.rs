//! The importer owns its configuration, preview witnesses and run checkpoints.
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::{
    StorageError, WorkImportConfig, WorkImportMapping, WorkImportMode, WorkImportRead,
    WorkImportRepository, WorkImportRun,
};
use devworks_core::protocol::{ActorId, Id, ProjectId};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Serialize, de::DeserializeOwned};

fn decode<T: DeserializeOwned>(row: &Row<'_>, column: usize) -> rusqlite::Result<T> {
    let json: String = row.get(column)?;
    serde_json::from_str(&json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn encode(record: &impl Serialize) -> Result<String, StorageError> {
    serde_json::to_string(record).map_err(|_| StorageError::InvalidRecord("import record".into()))
}

fn identifier(raw: String) -> Result<Id, StorageError> {
    Id::new(raw).map_err(|_| StorageError::InvalidRecord("import identity".into()))
}

fn lookup<T: DeserializeOwned>(
    conn: &Connection,
    sql: &str,
    values: impl rusqlite::Params,
) -> Result<Option<T>, StorageError> {
    db(conn.query_row(sql, values, |row| decode(row, 0)).optional())
}

fn mapping(
    conn: &Connection,
    namespace: &Id,
    kind: &str,
    source: &Id,
) -> Result<Option<WorkImportMapping>, StorageError> {
    let row = db(conn
        .query_row(
            "SELECT local_id, source_digest, destination_revision, project_id, observed_through_ms FROM work_import_mappings
             WHERE import_id=?1 AND source_kind=?2 AND source_id=?3",
            params![namespace.as_str(), kind, source.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional())?;
    row.map(
        |(local_id, source_digest, destination_revision, project, observed_through_ms)| {
            Ok(WorkImportMapping {
                source_namespace: namespace.clone(),
                source_kind: kind.to_owned(),
                source_id: source.clone(),
                source_digest,
                observed_through_ms,
                destination_id: identifier(local_id)?,
                destination_revision,
                project_id: project.map(identifier).transpose()?.map(ProjectId),
            })
        },
    )
    .transpose()
}

macro_rules! read_imports {
    ($typ:ty) => {
        impl WorkImportRead for $typ {
            fn import_config(&self, id: &Id) -> Result<Option<WorkImportConfig>, StorageError> {
                lookup(self.conn, "SELECT record_json FROM work_import_configs WHERE id=?1", [id.as_str()])
            }
            fn import_settings(&self, owner: &ActorId, namespace: &Id)
                -> Result<Option<WorkImportConfig>, StorageError>
            {
                lookup(self.conn, "SELECT record_json FROM work_import_configs WHERE owner=?1 AND source_namespace=?2",
                    params![owner.0.as_str(), namespace.as_str()])
            }
            fn import_run(&self, id: &Id) -> Result<Option<WorkImportRun>, StorageError> {
                lookup(self.conn, "SELECT record_json FROM work_import_runs WHERE id=?1", [id.as_str()])
            }
            fn import_preview(&self, owner: &ActorId, digest: &str)
                -> Result<Option<WorkImportRun>, StorageError>
            {
                lookup(self.conn, "SELECT record_json FROM work_import_runs
                    WHERE owner=?1 AND preview_digest=?2 AND is_preview=1 ORDER BY sequence DESC LIMIT 1",
                    params![owner.0.as_str(), digest])
            }
            fn matching_preview(&self, owner: &ActorId, namespace: &Id, scope: &str, snapshot: &str)
                -> Result<Option<WorkImportRun>, StorageError>
            {
                lookup(self.conn, "SELECT record_json FROM work_import_runs WHERE owner=?1
                    AND source_namespace=?2 AND scope_digest=?3 AND source_snapshot=?4
                    AND is_preview=1 ORDER BY sequence DESC LIMIT 1",
                    params![owner.0.as_str(), namespace.as_str(), scope, snapshot])
            }
            fn import_runs(&self, owner: &ActorId, namespace: Option<&Id>, before: Option<i64>, limit: u32)
                -> Result<Vec<(i64, WorkImportRun)>, StorageError>
            {
                let mut stmt = db(self.conn.prepare(
                    "SELECT sequence, record_json FROM work_import_runs
                     WHERE owner=?1 AND (?2 IS NULL OR source_namespace=?2)
                     AND sequence < ?3 ORDER BY sequence DESC LIMIT ?4"))?;
                let rows = db(stmt.query_map(
                    params![owner.0.as_str(), namespace.map(Id::as_str), before.unwrap_or(i64::MAX), limit.clamp(1,501)],
                    |row| Ok((row.get(0)?, decode(row, 1)?)),
                ))?;
                db(rows.collect())
            }
            fn active_import(&self, namespace: &Id) -> Result<Option<WorkImportRun>, StorageError>
            {
                lookup(self.conn, "SELECT record_json FROM work_import_runs WHERE source_namespace=?1
                    AND is_preview=0 AND completed_at_ms IS NULL ORDER BY sequence DESC LIMIT 1",
                    params![namespace.as_str()])
            }
            fn latest_import(&self, owner: &ActorId, namespace: &Id, scope_digest: &str, revision: Option<i64>)
                -> Result<Option<WorkImportRun>, StorageError>
            {
                lookup(self.conn, "SELECT record_json FROM work_import_runs
                    WHERE owner=?1 AND source_namespace=?2 AND scope_digest=?3 AND is_preview=0
                    AND completed_at_ms IS NOT NULL AND json_extract(record_json,'$.state')='ObservedComplete'
                    AND json_extract(record_json,'$.config_revision') IS ?4
                    ORDER BY json_extract(record_json,'$.captured_through_ms') DESC, sequence DESC LIMIT 1",
                    params![owner.0.as_str(), namespace.as_str(), scope_digest, revision])
            }
            fn latest_import_issues(&self, owner: &ActorId, namespace: &Id, scope_digest: &str, revision: Option<i64>)
                -> Result<Option<WorkImportRun>, StorageError>
            {
                lookup(self.conn,"SELECT record_json FROM work_import_runs WHERE
                    owner=?1 AND source_namespace=?2 AND scope_digest=?3 AND is_preview=0
                    AND completed_at_ms IS NOT NULL AND json_extract(record_json,'$.config_revision') IS ?4
                    ORDER BY sequence DESC LIMIT 1",
                    params![owner.0.as_str(), namespace.as_str(), scope_digest, revision])
            }
            fn import_mapping(&self, namespace: &Id, kind: &str, source_id: &Id)
                -> Result<Option<WorkImportMapping>, StorageError>
            {
                mapping(self.conn, namespace, kind, source_id)
            }
        }
    };
}
read_imports!(Snapshot<'_>);
read_imports!(Unit<'_>);

fn advance(revision: i64, expected: Option<i64>) -> Result<(), StorageError> {
    if expected.unwrap_or(0).checked_add(1) == Some(revision) {
        return Ok(());
    }
    Err(StorageError::InvalidRecord("import revision".into()))
}

impl WorkImportRepository for Unit<'_> {
    fn put_import_config(
        &mut self,
        record: &WorkImportConfig,
        expected_revision: Option<i64>,
    ) -> Result<(), StorageError> {
        advance(record.revision, expected_revision)?;
        let data = encode(record)?;
        let affected = match expected_revision {
            None => db(self.conn.execute(
                "INSERT INTO work_import_configs(id, owner, source_namespace, revision, record_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![record.id.as_str(), record.owner.0.as_str(), record.source_namespace.as_str(), record.revision, data],
            ))?,
            Some(prior) => db(self.conn.execute(
                "UPDATE work_import_configs SET revision=?4, record_json=?5
                 WHERE id=?1 AND owner=?2 AND source_namespace=?3 AND revision=?6",
                params![record.id.as_str(), record.owner.0.as_str(), record.source_namespace.as_str(), record.revision, data, prior],
            ))?,
        };
        if affected != 1 {
            return Err(StorageError::Duplicate {
                kind: "work_import_config",
                id: record.id.as_str().into(),
            });
        }
        Ok(())
    }

    fn put_import_run(
        &mut self,
        record: &WorkImportRun,
        expected_revision: Option<i64>,
    ) -> Result<(), StorageError> {
        advance(record.revision, expected_revision)?;
        let data = encode(record)?;
        let affected = match expected_revision {
            None => db(self.conn.execute(
                "INSERT INTO work_import_runs(id, owner, source_namespace, revision, source_snapshot,
                    scope_digest, preview_digest, is_preview, completed_at_ms, record_json)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![record.id.as_str(), record.owner.0.as_str(), record.source_namespace.as_str(),
                    record.revision, record.source_snapshot, record.scope_digest, record.preview_digest,
                    record.mode == WorkImportMode::DryRun, record.completed_at_ms, data],
            ))?,
            Some(prior) => db(self.conn.execute(
                "UPDATE work_import_runs SET revision=?3, completed_at_ms=?4, record_json=?5
                 WHERE id=?1 AND owner=?2 AND revision=?6",
                params![record.id.as_str(), record.owner.0.as_str(), record.revision,
                    record.completed_at_ms, data, prior],
            ))?,
        };
        if affected != 1 {
            return Err(StorageError::Duplicate {
                kind: "work_import_run",
                id: record.id.as_str().into(),
            });
        }
        Ok(())
    }

    fn put_import_mapping(&mut self, record: &WorkImportMapping) -> Result<(), StorageError> {
        let changed = db(self.conn.execute(
            "INSERT INTO work_import_mappings(import_id,source_kind,source_id,local_id,source_digest,
                destination_revision,project_id,observed_through_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(import_id,source_kind,source_id) DO UPDATE SET source_digest=excluded.source_digest,
                destination_revision=excluded.destination_revision, observed_through_ms=excluded.observed_through_ms
             WHERE work_import_mappings.local_id=excluded.local_id AND work_import_mappings.project_id IS excluded.project_id",
            params![record.source_namespace.as_str(), record.source_kind, record.source_id.as_str(),
                record.destination_id.as_str(), record.source_digest, record.destination_revision,
                record.project_id.as_ref().map(|p|p.0.as_str()),record.observed_through_ms],
        ))?;
        if changed != 1 {
            return Err(StorageError::Duplicate {
                kind: "work_import_mapping",
                id: record.source_id.as_str().into(),
            });
        }
        Ok(())
    }
}
