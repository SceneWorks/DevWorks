//! SQLite owns retention selection and commits exact removals with their durable outcomes.
use super::conn::db;
use super::repos::{Snapshot, Unit};
use super::retention_policy;
use super::support::{conflict, corrupt, id_of, invalid, page_limit};
use devworks_core::ports::storage::{
    RetentionHold, RetentionKind, RetentionOutcome, RetentionRead, RetentionRepository,
    RetentionSelection, RetentionWindow, RetentionWitness, StorageError, StoredRetention,
};
use devworks_core::protocol::{Id, OperationId, RevisionDimension};
use rusqlite::{Connection, OptionalExtension, Row, named_params, params};

const HANDOVER_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

fn lookup(
    conn: &Connection,
    column: &str,
    key: &Id,
) -> Result<Option<StoredRetention>, StorageError> {
    let sql = format!("SELECT record_json FROM operational_retention_previews WHERE {column}=?1");
    let raw: Option<String> = db(db(conn.prepare_cached(&sql))?
        .query_row([key.as_str()], |row| row.get(0))
        .optional())?;
    raw.map(|raw| {
        serde_json::from_str(&raw).map_err(|error| corrupt(format!("retention preview: {error}")))
    })
    .transpose()
}

type RawWitness = (String, i64, i64, Option<String>);

fn raw_witness(row: &Row<'_>) -> rusqlite::Result<RawWitness> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

fn witness(
    (id, revision, activity_ms, hold): RawWitness,
) -> Result<RetentionWitness, StorageError> {
    let hold = hold
        .map(|hold| {
            serde_json::from_value::<RetentionHold>(serde_json::Value::String(hold))
                .map_err(|error| corrupt(format!("retention hold: {error}")))
        })
        .transpose()?;
    Ok(RetentionWitness {
        id: id_of(id)?,
        revision,
        activity_ms,
        hold,
    })
}

fn floor(window: &RetentionWindow) -> i64 {
    window.captured_at_ms.saturating_sub(HANDOVER_MS).max(0)
}

fn preview(
    conn: &Connection,
    window: &RetentionWindow,
) -> Result<RetentionSelection, StorageError> {
    let limit = page_limit(window.limit)?;
    let select = retention_policy::select(window.kind, "1=1");
    let query = format!(
        "SELECT * FROM ({select}) WHERE (hold IS NOT NULL)=:held ORDER BY activity,id LIMIT :limit"
    );
    let mut stmt = db(conn.prepare_cached(&query))?;
    let mut page = |held: bool| {
        let mut rows = db(stmt.query_map(
            named_params! {
              ":cutoff": window.effective_cutoff_ms,
              ":floor": floor(window),
              ":held": held,
              ":limit": limit + 1,
            },
            raw_witness,
        ))?
        .map(|row| witness(db(row)?))
        .collect::<Result<Vec<_>, StorageError>>()?;
        let more = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        Ok::<_, StorageError>((rows, more))
    };
    let (candidates, more_candidates) = page(false)?;
    let (held, more_held) = page(true)?;
    Ok(RetentionSelection {
        candidates,
        held,
        more_candidates,
        more_held,
    })
}

fn read_exact(
    conn: &Connection,
    window: &RetentionWindow,
    ids: &[Id],
) -> Result<Vec<RetentionWitness>, StorageError> {
    if ids.len() > page_limit(window.limit)? as usize {
        return Err(invalid("retention witness list exceeds its approved limit"));
    }
    let sql = retention_policy::select(window.kind, "r.id=:id");
    let mut stmt = db(conn.prepare_cached(&sql))?;
    let mut rows = Vec::with_capacity(ids.len());
    for id in ids {
        let row = db(stmt
            .query_row(
                named_params! {
                    ":cutoff": window.effective_cutoff_ms,
                    ":floor": floor(window),
                    ":id": id.as_str(),
                },
                raw_witness,
            )
            .optional())?;
        if let Some(row) = row {
            rows.push(witness(row)?);
        }
    }
    Ok(rows)
}

macro_rules! read_retention {
    ($ty:ty) => {
        impl RetentionRead for $ty {
            fn retention_preview(&self, id: &Id) -> Result<Option<StoredRetention>, StorageError> {
                lookup(self.conn, "id", id)
            }
            fn retention_for_job_run(
                &self,
                id: &Id,
            ) -> Result<Option<StoredRetention>, StorageError> {
                lookup(self.conn, "native_run_id", id)
            }
            fn retention_for_operation(
                &self,
                id: &OperationId,
            ) -> Result<Option<StoredRetention>, StorageError> {
                lookup(self.conn, "operation_id", &id.0)
            }
            fn preview_retention(
                &self,
                window: &RetentionWindow,
            ) -> Result<RetentionSelection, StorageError> {
                preview(self.conn, window)
            }
            fn retention_witnesses(
                &self,
                window: &RetentionWindow,
                ids: &[Id],
            ) -> Result<Vec<RetentionWitness>, StorageError> {
                read_exact(self.conn, window, ids)
            }
        }
    };
}

read_retention!(Snapshot<'_>);
read_retention!(Unit<'_>);

impl RetentionRepository for Unit<'_> {
    fn put_retention_preview(
        &mut self,
        value: &StoredRetention,
        expected_revision: Option<i64>,
    ) -> Result<(), StorageError> {
        let current = self.retention_preview(&value.id)?;
        match (&current, expected_revision) {
            (None, None) if value.revision == 1 => {}
            (Some(previous), Some(expected)) if previous.revision == expected => {
                if value.revision.checked_sub(1) != Some(expected)
                    || previous.owner != value.owner
                    || previous.store != value.store
                    || previous.restore_epoch != value.restore_epoch
                    || previous.window != value.window
                    || previous.expires_at_ms != value.expires_at_ms
                    || previous.selection != value.selection
                    || previous.scheduled != value.scheduled
                    || (previous.operation.is_some() && previous.operation != value.operation)
                {
                    return Err(invalid(
                        "retention preview scope and selection are immutable",
                    ));
                }
            }
            (Some(previous), _) => {
                return Err(conflict(
                    RevisionDimension::Reference,
                    "retention-preview",
                    value.id.as_str(),
                    expected_revision.unwrap_or(1),
                    previous.revision,
                ));
            }
            (None, _) => {
                return Err(StorageError::NotFound {
                    kind: "retention-preview",
                    id: value.id.as_str().to_owned(),
                });
            }
        }
        let json = serde_json::to_string(value)
            .map_err(|_| invalid("retention preview cannot be encoded"))?;
        db(self.conn.execute(
            "INSERT INTO operational_retention_previews
             (id,owner,kind,revision,expires_at_ms,native_run_id,operation_id,record_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(id) DO UPDATE SET revision=excluded.revision,
               operation_id=excluded.operation_id,record_json=excluded.record_json",
            params![
                value.id.as_str(),
                value.owner.0.as_str(),
                value.window.kind.as_str(),
                value.revision,
                value.expires_at_ms,
                value.scheduled.as_ref().map(|run| run.run_id.as_str()),
                value.operation.as_ref().map(|op| op.0.as_str()),
                json,
            ],
        ))?;
        Ok(())
    }

    fn apply_retention(
        &mut self,
        window: &RetentionWindow,
        selected: &[RetentionWitness],
    ) -> Result<RetentionOutcome, StorageError> {
        let ids = selected
            .iter()
            .map(|row| row.id.clone())
            .collect::<Vec<_>>();
        let observed = self.retention_witnesses(window, &ids)?;
        if observed != selected || observed.iter().any(|row| row.hold.is_some()) {
            return Err(invalid("retention candidates changed after preview"));
        }
        let mut removed = 0;
        let mut children = 0;
        let mut diagnostic_boundary = None;
        let delete = format!(
            "DELETE FROM {} WHERE id=?1",
            retention_policy::table(window.kind)
        );
        for row in selected {
            let id = row.id.as_str();
            let conversation: Option<String> = if window.kind == RetentionKind::Sessions {
                Some(db(self.conn.query_row(
                    "SELECT conversation_id FROM sessions WHERE id=?1",
                    [id],
                    |row| row.get(0),
                ))?)
            } else {
                None
            };
            if window.kind == RetentionKind::Messages {
                children += db(self.conn.execute(
                    "DELETE FROM agent_message_deliveries WHERE message_id=?1",
                    [id],
                ))?;
                children += db(self
                    .conn
                    .execute("DELETE FROM agent_message_events WHERE message_id=?1", [id]))?;
            }
            if db(db(self.conn.prepare_cached(&delete))?.execute([id]))? != 1 {
                return Err(corrupt(
                    "retention row disappeared in the write transaction",
                ));
            }
            removed += 1;
            if window.kind == RetentionKind::Diagnostics {
                diagnostic_boundary = diagnostic_boundary.max(Some(row.revision));
            }
            if let Some(conversation) = conversation {
                children += db(self.conn.execute(
                    "DELETE FROM conversations WHERE id=?1 AND root_id IS NULL
                     AND NOT EXISTS (SELECT 1 FROM sessions s WHERE s.conversation_id=?1)",
                    [&conversation],
                ))?;
            }
        }
        if let Some(boundary) = diagnostic_boundary {
            raise_diagnostic_boundary(self.conn, boundary)?;
        }
        Ok(RetentionOutcome {
            records_removed: removed,
            owned_rows_removed: children as i64,
            ..RetentionOutcome::default()
        })
    }
}

pub(super) fn raise_diagnostic_boundary(
    conn: &Connection,
    sequence: i64,
) -> Result<(), StorageError> {
    db(conn.execute(
        "UPDATE diagnostic_retention SET pruned_through=max(pruned_through,?1) WHERE singleton=1",
        [sequence],
    ))?;
    Ok(())
}
