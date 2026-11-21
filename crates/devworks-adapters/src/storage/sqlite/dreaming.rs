//! SQLite transactions fence Dreaming progress and advance a completed cutoff with its report.
use super::conn::db;
use super::repos::{Snapshot, Unit};
use super::support::{conflict, corrupt, id_of, invalid, page_limit};
use devworks_core::ports::storage::{
    DreamActionRow, DreamCheckpoint, DreamClaimRef, DreamClaimWindow, DreamCutoffs,
    DreamInstruction, DreamPhase, DreamPolicy, DreamProgress, DreamRequestStage, DreamRunRow,
    DreamScopeState, DreamSignal, DreamStream, DreamTrigger, DreamingRead, DreamingRepository,
    KnowledgeScope, StorageError,
};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::{ActorId, Id, OperationId, RevisionDimension};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

fn encode(value: &impl Serialize) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|error| invalid(error.to_string()))
}

fn decode<T: DeserializeOwned>(value: &str) -> Result<T, StorageError> {
    serde_json::from_str(value).map_err(|error| corrupt(error.to_string()))
}

fn decode_optional<T: DeserializeOwned>(
    row: &Row<'_>,
    column: usize,
) -> Result<Option<T>, StorageError> {
    db(row.get::<_, Option<String>>(column))?
        .as_deref()
        .map(decode)
        .transpose()
}

fn token(value: &impl Serialize) -> Result<String, StorageError> {
    match serde_json::to_value(value).map_err(|error| invalid(error.to_string()))? {
        Value::String(text) => Ok(text),
        _ => Err(invalid("invalid Dreaming state token")),
    }
}

fn phase<T: DeserializeOwned>(row: &Row<'_>, column: usize) -> Result<T, StorageError> {
    serde_json::from_value(Value::String(db(row.get(column))?))
        .map_err(|err| corrupt(err.to_string()))
}

fn not_found(run: &Id) -> StorageError {
    StorageError::NotFound {
        kind: "dreaming_run",
        id: run.as_str().to_owned(),
    }
}

fn lease_lost(run: &Id) -> StorageError {
    StorageError::LeaseLost {
        kind: "dreaming_run",
        id: run.as_str().to_owned(),
    }
}

fn state(conn: &Connection, scope: &KnowledgeScope) -> Result<DreamScopeState, StorageError> {
    let row = db(conn
        .query_row(
            "SELECT settings_revision,settings_json,completed_cutoffs_json,completed_at
           FROM dreaming_scope_state WHERE scope_key=?1",
            [scope.key()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get(3)?,
                ))
            },
        )
        .optional())?;
    let Some((revision, policy, cutoffs, completed_at)) = row else {
        return Ok(DreamScopeState::default());
    };
    let completed = match (cutoffs, completed_at) {
        (Some(cutoffs), Some(at)) => Some((decode(&cutoffs)?, at)),
        (None, None) => None,
        _ => {
            return Err(corrupt(
                "Dreaming completed cutoff is missing its timestamp",
            ));
        }
    };
    Ok(DreamScopeState {
        revision,
        policy: decode(&policy)?,
        completed,
    })
}

const RUN_COLUMNS: &str = "id,scope_key,continued_from,operation_id,initiator_id,trigger,revision,    generation,state,settings_revision,settings_json,cutoffs_json,started_at,ended_at,    checkpoint_json,report_json";

fn run_row(row: &Row<'_>) -> Result<DreamRunRow, StorageError> {
    let id = |index| db(row.get::<_, String>(index)).and_then(id_of);
    let optional_id = |index| {
        db(row.get::<_, Option<String>>(index))?
            .map(id_of)
            .transpose()
    };
    Ok(DreamRunRow {
        id: id(0)?,
        scope: KnowledgeScope::parse(&db(row.get::<_, String>(1))?)?,
        continued_from: optional_id(2)?,
        operation_id: optional_id(3)?.map(OperationId),
        initiator: ActorId(id(4)?),
        trigger: phase(row, 5)?,
        revision: db(row.get(6))?,
        generation: db(row.get(7))?,
        phase: phase(row, 8)?,
        settings_revision: db(row.get(9))?,
        policy: decode(&db(row.get::<_, String>(10))?)?,
        cutoffs: decode(&db(row.get::<_, String>(11))?)?,
        started_at_ms: db(row.get(12))?,
        ended_at_ms: db(row.get(13))?,
        checkpoint: decode_optional(row, 14)?,
        report: decode_optional(row, 15)?,
    })
}

fn run(conn: &Connection, id: &Id) -> Result<Option<DreamRunRow>, StorageError> {
    let mut statement = db(conn.prepare_cached(&format!(
        "SELECT {RUN_COLUMNS} FROM dreaming_runs WHERE id=?1"
    )))?;
    let mut rows = db(statement.query([id.as_str()]))?;
    db(rows.next())?.map(run_row).transpose()
}

fn live_run(
    conn: &Connection,
    scope: &KnowledgeScope,
) -> Result<Option<DreamRunRow>, StorageError> {
    let mut statement = db(conn.prepare_cached(&format!(
        "SELECT {RUN_COLUMNS} FROM dreaming_runs
         WHERE scope_key=?1 AND state IN ('Queued','Running','OutcomeUnknown')"
    )))?;
    let mut rows = db(statement.query([scope.key()]))?;
    db(rows.next())?.map(run_row).transpose()
}

fn action_row(row: &Row<'_>) -> Result<DreamActionRow, StorageError> {
    Ok(DreamActionRow {
        id: id_of(db(row.get(0))?)?,
        position: db(row.get(1))?,
        committed_at_ms: db(row.get(2))?,
        action: decode(&db(row.get::<_, String>(3))?)?,
    })
}

fn event_tip(
    conn: &Connection,
    scope: &KnowledgeScope,
    stream: DreamStream,
) -> Result<i64, StorageError> {
    let sql = match stream {
        DreamStream::Knowledge => {
            "SELECT coalesce(max(seq),0) FROM events WHERE project_id IS ?1 AND kind='KnowledgeWritten'"
        }
        DreamStream::Reviews => {
            "SELECT coalesce(max(seq),0) FROM events WHERE project_id IS ?1 AND kind='PlanReviewSettled'"
        }
        DreamStream::Sources => {
            return db(conn.query_row(
                "SELECT coalesce(max(seq),0) FROM dreaming_source_changes WHERE scope_key=?1",
                [scope.key()],
                |row| row.get(0),
            ));
        }
    };
    db(
        conn.query_row(sql, [scope.project().map(|id| id.0.as_str())], |row| {
            row.get(0)
        }),
    )
}

fn instruction_tip(conn: &Connection, scope: &KnowledgeScope) -> Result<i64, StorageError> {
    db(conn.query_row(
        "SELECT coalesce(max(revision),0) FROM dreaming_instruction_revisions WHERE scope_key=?1",
        [scope.key()],
        |row| row.get(0),
    ))
}

macro_rules! dreaming_reads {
    ($reader:ty) => {
        impl DreamingRead for $reader {
            fn dream_scope(&self, scope: &KnowledgeScope) -> Result<DreamScopeState, StorageError> {
                state(self.conn, scope)
            }

            fn dream_cutoffs(&self, scope: &KnowledgeScope) -> Result<DreamCutoffs, StorageError> {
                let base = &KnowledgeScope::Workspace;
                let mut cutoffs = DreamCutoffs {
                    knowledge: event_tip(self.conn, scope, DreamStream::Knowledge)?,
                    reviews: event_tip(self.conn, scope, DreamStream::Reviews)?,
                    sources: event_tip(self.conn, scope, DreamStream::Sources)?,
                    base_instruction_revision: instruction_tip(self.conn, base)?,
                    ..Default::default()
                };
                if scope.project().is_some() {
                    cutoffs.workspace_knowledge = event_tip(self.conn, base, DreamStream::Knowledge)?;
                    cutoffs.workspace_sources = event_tip(self.conn, base, DreamStream::Sources)?;
                    cutoffs.project_instruction_revision = instruction_tip(self.conn, scope)?;
                }
                Ok(cutoffs)
            }

            fn dream_signals(
                &self, scope: &KnowledgeScope, stream: DreamStream, after: i64, through: i64,
                limit: u16,
            ) -> Result<Vec<DreamSignal>, StorageError> {
                let key = scope.key();
                let project = scope.project().map(|id| id.0.as_str());
                let (sql, scope_filter) = match stream {
                    DreamStream::Knowledge => ("SELECT e.seq,e.id,e.created_at,NULL FROM events e WHERE e.project_id IS ?1
                        AND e.kind='KnowledgeWritten' AND e.seq>?2 AND e.seq<=?3
                        AND NOT EXISTS (
                            SELECT 1 FROM knowledge_writes w
                              JOIN dreaming_runs d ON d.id=w.session_id AND d.scope_key=w.scope_key
                              JOIN actors a ON a.id=w.actor_id AND a.kind='Host'
                            WHERE w.write_id=e.aggregate_id
                               AND d.state='Completed'
                        ) ORDER BY e.seq LIMIT ?4", project),
                    DreamStream::Reviews => ("SELECT seq,id,created_at,NULL FROM events WHERE project_id IS ?1
                        AND kind='PlanReviewSettled' AND seq>?2 AND seq<=?3 ORDER BY seq LIMIT ?4", project),
                    DreamStream::Sources => ("SELECT seq,source_ref,observed_at,source_revision FROM dreaming_source_changes
                        WHERE scope_key=?1 AND seq>?2 AND seq<=?3 ORDER BY seq LIMIT ?4", Some(key.as_str())),
                };
                let mut statement = db(self.conn.prepare_cached(sql))?;
                let rows = db(statement.query_map(params![scope_filter, after, through, page_limit(limit)?],
                    |row| Ok(DreamSignal {
                        sequence: row.get(0)?, reference: row.get(1)?, observed_at_ms: row.get(2)?,
                        source_revision: row.get(3)?,
                    })
                ))?;
                rows.map(db).collect()
            }

            fn dream_claim_refs(&self, window: &DreamClaimWindow<'_>)
                -> Result<Vec<DreamClaimRef>, StorageError>
            {
                let mut statement = db(self.conn.prepare_cached(
                    "SELECT claim.id, frozen.revision, claim.revision, frozen.lifecycle,
                        coalesce(json_extract(frozen.snapshot_json,'$.entryKind'),json_extract(frozen.snapshot_json,'$.kind'))
                       FROM knowledge_claims claim
                       JOIN knowledge_history frozen ON frozen.target_id=claim.id
                            AND frozen.revision=(
                                SELECT h.revision FROM knowledge_history h
                                 JOIN events e ON e.aggregate_kind='knowledge_write'
                                    AND e.aggregate_id=h.write_id AND e.kind='KnowledgeWritten'
                                 WHERE h.target_id=claim.id AND e.seq<=?2
                                    AND e.project_id IS ?7
                                 ORDER BY h.revision DESC LIMIT 1)
                      WHERE claim.scope_key=?1 AND claim.id>?3 AND claim.lifecycle<>'Deleted'
                        AND frozen.lifecycle<>'Deleted'
                        AND (?4 IS NULL OR claim.id=?4)
                        AND (?5 IS NULL OR coalesce(json_extract(frozen.snapshot_json,'$.entryKind'),
                            json_extract(frozen.snapshot_json,'$.kind'))=?5)
                        AND (?6 IS NULL OR frozen.lifecycle=?6)
                      ORDER BY claim.id LIMIT ?8",
                ))?;
                let mut rows = db(statement.query(params![
                    window.scope.key(),window.through,window.after.map_or("",Id::as_str),
                    window.exact.map(Id::as_str),window.kind,window.status,
                    window.scope.project().map(|project| project.0.as_str()),page_limit(window.limit)?,
                ]))?;
                let mut result = Vec::new();
                while let Some(row) = db(rows.next())? {
                    result.push(DreamClaimRef { id:id_of(db(row.get(0))?)?,
                        revision:db(row.get(1))?,current_revision:db(row.get(2))?,
                        lifecycle:db(row.get(3))?,entry_kind:db(row.get(4))? });
                }
                Ok(result)
            }

            fn dream_instructions(
                &self, scope: &KnowledgeScope, before_revision: Option<i64>, limit: u16,
            ) -> Result<Vec<DreamInstruction>, StorageError> {
                let mut statement = db(self.conn.prepare_cached(
                    "SELECT revision,editor_id,created_at,artifact_json
                     FROM dreaming_instruction_revisions
                     WHERE scope_key=?1 AND revision<?2 ORDER BY revision DESC LIMIT ?3",
                ))?;
                let mut rows = db(statement.query(params![
                    scope.key(), before_revision.unwrap_or(i64::MAX), page_limit(limit)?,
                ]))?;
                let mut result = Vec::new();
                while let Some(row) = db(rows.next())? {
                    result.push(DreamInstruction {
                        revision: db(row.get(0))?,
                        editor: ActorId(id_of(db(row.get(1))?)?),
                        created_at_ms: db(row.get(2))?,
                        artifact: decode(&db(row.get::<_, String>(3))?)?,
                    });
                }
                Ok(result)
            }

            fn dream_run(&self, id: &Id) -> Result<Option<DreamRunRow>, StorageError> {
                run(self.conn, id)
            }

            fn dream_live_run(&self, scope: &KnowledgeScope) -> Result<Option<DreamRunRow>, StorageError> {
                live_run(self.conn, scope)
            }

            fn dream_runs(
                &self, scope: &KnowledgeScope, before: Option<(i64, &Id)>, limit: u16,
            ) -> Result<Vec<DreamRunRow>, StorageError> {
                // Keep both branches on the same index and exclude the previous page's tail.
                let sql = if before.is_some() {
                    format!("SELECT {RUN_COLUMNS} FROM dreaming_runs WHERE scope_key=?1
                             AND (started_at,id)<(?2,?3) ORDER BY started_at DESC,id DESC LIMIT ?4")
                } else {
                    format!("SELECT {RUN_COLUMNS} FROM dreaming_runs WHERE scope_key=?1
                             ORDER BY started_at DESC,id DESC LIMIT ?4")
                };
                let mut statement = db(self.conn.prepare_cached(&sql))?;
                let mut rows = db(statement.query(params![
                    scope.key(), before.map(|(at, _)| at), before.map(|(_, id)| id.as_str()),
                    page_limit(limit)?,
                ]))?;
                let mut result = Vec::new();
                while let Some(row) = db(rows.next())? {
                    result.push(run_row(row)?);
                }
                Ok(result)
            }

            fn dream_action(&self, run: &Id, action: &Id) -> Result<Option<DreamActionRow>, StorageError> {
                let mut statement = db(self.conn.prepare_cached(
                    "SELECT action_id,position,committed_at,action_json FROM dreaming_actions
                     WHERE run_id=?1 AND action_id=?2",
                ))?;
                let mut rows = db(statement.query(params![run.as_str(), action.as_str()]))?;
                db(rows.next())?.map(action_row).transpose()
            }

            fn dream_reversion(&self, run: &Id, action: &Id) -> Result<Option<Value>, StorageError> {
                let receipt = db(self.conn.query_row(
                    "SELECT receipt_json FROM dreaming_reversions WHERE run_id=?1 AND action_id=?2",
                    params![run.as_str(), action.as_str()], |row| row.get::<_, String>(0),
                ).optional())?;
                receipt.as_deref().map(decode).transpose()
            }

            fn dream_actions(
                &self, run: &Id, before: Option<i64>, limit: u16,
            ) -> Result<Vec<DreamActionRow>, StorageError> {
                let mut statement = db(self.conn.prepare_cached(
                    "SELECT action_id,position,committed_at,action_json FROM dreaming_actions
                     WHERE run_id=?1 AND position<?2 ORDER BY position DESC LIMIT ?3",
                ))?;
                let mut rows = db(statement.query(params![
                    run.as_str(), before.unwrap_or(i64::MAX), page_limit(limit)?,
                ]))?;
                let mut result = Vec::new();
                while let Some(row) = db(rows.next())? {
                    result.push(action_row(row)?);
                }
                Ok(result)
            }
        }
    };
}

dreaming_reads!(Snapshot<'_>);
dreaming_reads!(Unit<'_>);

fn revision_conflict(kind: &'static str, key: &str, expected: i64, actual: i64) -> StorageError {
    conflict(
        RevisionDimension::Operation,
        kind,
        key,
        expected,
        actual.max(1),
    )
}

fn next_revision(revision: i64) -> Result<i64, StorageError> {
    revision
        .checked_add(1)
        .ok_or_else(|| invalid("Dreaming revision exhausted"))
}

fn validate_checkpoint(
    prior: Option<&DreamCheckpoint>,
    next: Option<&DreamCheckpoint>,
) -> Result<(), StorageError> {
    let prior_request = prior.and_then(|checkpoint| checkpoint.request.as_ref());
    let next_request = next.and_then(|checkpoint| checkpoint.request.as_ref());
    if let (Some(prior), Some(next)) = (prior, next)
        && (prior.turn > next.turn
            || prior.input_tokens > next.input_tokens
            || prior.output_tokens > next.output_tokens
            || prior.priced_micro_usd > next.priced_micro_usd
            || prior
                .cursor_key
                .is_some_and(|old| next.cursor_key != Some(old)))
    {
        return Err(invalid("Dreaming checkpoint counters went backwards"));
    }
    if let Some(request) = next_request
        && ((request.stage == DreamRequestStage::Observed) != request.result.is_some()
            || (request.stage != DreamRequestStage::Prepared && request.attempt_id.is_none()))
    {
        return Err(invalid("Dreaming provider observation is incomplete"));
    }
    match (prior_request, next_request) {
        (Some(old), Some(new)) if old.logical_id == new.logical_id => {
            let legal_stage = old.stage == new.stage
                || matches!(
                    (&old.stage, &new.stage),
                    (DreamRequestStage::Prepared, DreamRequestStage::PossiblySent)
                        | (
                            DreamRequestStage::PossiblySent | DreamRequestStage::Prepared,
                            DreamRequestStage::Observed
                        )
                );
            if !legal_stage
                || old
                    .attempt_id
                    .as_ref()
                    .is_some_and(|attempt| Some(attempt) != new.attempt_id.as_ref())
                || old.frame != new.frame
                || old
                    .result
                    .as_ref()
                    .is_some_and(|old| Some(old) != new.result.as_ref())
            {
                return Err(invalid("Dreaming provider request identity changed"));
            }
        }
        (Some(old), _) if old.stage == DreamRequestStage::PossiblySent => {
            return Err(invalid("Dreaming cannot replace a possibly sent request"));
        }
        (_, Some(new)) if new.stage != DreamRequestStage::Prepared => {
            return Err(invalid("Dreaming provider request was not prepared"));
        }
        _ => {}
    }
    Ok(())
}

fn validate_progress(old: &DreamRunRow, next: &DreamProgress) -> Result<(), StorageError> {
    use DreamPhase::*;
    let allowed = match old.phase {
        Queued => matches!(next.phase, Running | Cancelled | Blocked | Failed),
        Running => matches!(
            next.phase,
            Running
                | Completed
                | Partial
                | Failed
                | Cancelled
                | Blocked
                | BudgetExhausted
                | OutcomeUnknown
        ),
        OutcomeUnknown => {
            next.phase == Running
                && next
                    .checkpoint
                    .as_ref()
                    .and_then(|c| c.request.as_ref())
                    .is_some_and(|request| request.stage == DreamRequestStage::Observed)
        }
        _ => false,
    };
    if !allowed {
        return Err(lease_lost(&old.id));
    }
    validate_checkpoint(old.checkpoint.as_ref(), next.checkpoint.as_ref())?;
    let uncertain = next
        .checkpoint
        .as_ref()
        .and_then(|c| c.request.as_ref())
        .is_some_and(|request| request.stage == DreamRequestStage::PossiblySent);
    if (next.phase == OutcomeUnknown && !uncertain)
        || (next.phase.ended() && next.phase != OutcomeUnknown && uncertain)
        || (next.phase.ended() && next.report.is_none())
    {
        return Err(invalid(
            "Dreaming terminal state lacks its report or provider outcome",
        ));
    }
    Ok(())
}

impl DreamingRepository for Unit<'_> {
    fn put_dream_reversion(
        &mut self,
        run: &Id,
        action: &Id,
        actor: &ActorId,
        receipt: &Value,
    ) -> Result<(), StorageError> {
        let row = self.dream_run(run)?.ok_or_else(|| not_found(run))?;
        if row.phase.owns_scope() {
            return Err(lease_lost(run));
        }
        db(self.conn.execute(
            "INSERT INTO dreaming_reversions(run_id,action_id,reverted_by,reverted_at,receipt_json)
             VALUES(?1,?2,?3,?4,?5)",
            params![
                run.as_str(),
                action.as_str(),
                actor.0.as_str(),
                self.now,
                encode(receipt)?
            ],
        ))?;
        Ok(())
    }

    fn set_dream_policy(
        &mut self,
        scope: &KnowledgeScope,
        expected: i64,
        policy: &DreamPolicy,
    ) -> Result<DreamScopeState, StorageError> {
        policy.validate()?;
        let prior = state(self.conn, scope)?;
        let key = scope.key();
        if prior.revision != expected {
            return Err(revision_conflict(
                "dreaming_settings",
                &key,
                expected,
                prior.revision,
            ));
        }
        db(self.conn.execute(
            "INSERT INTO dreaming_scope_state(scope_key,project_id,settings_revision,settings_json)
             VALUES(?1,?2,?3,?4)
             ON CONFLICT(scope_key) DO UPDATE SET
                 settings_revision=excluded.settings_revision,settings_json=excluded.settings_json",
            params![
                key,
                scope.project().map(|id| id.0.as_str()),
                next_revision(expected)?,
                encode(policy)?
            ],
        ))?;
        state(self.conn, scope)
    }

    fn add_dream_instruction(
        &mut self,
        scope: &KnowledgeScope,
        expected: i64,
        artifact: &ArtifactRef,
        editor: &ActorId,
    ) -> Result<DreamInstruction, StorageError> {
        let key = scope.key();
        let current = instruction_tip(self.conn, scope)?;
        if current != expected {
            return Err(revision_conflict(
                "dreaming_instructions",
                &key,
                expected,
                current,
            ));
        }
        let revision = next_revision(current)?;
        db(self.conn.execute(
            "INSERT INTO dreaming_instruction_revisions(scope_key,project_id,revision,editor_id,
                 created_at,artifact_version_id,artifact_json) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                key,
                scope.project().map(|id| id.0.as_str()),
                revision,
                editor.0.as_str(),
                self.now,
                artifact.version_id.as_str(),
                encode(artifact)?
            ],
        ))?;
        Ok(DreamInstruction {
            revision,
            editor: editor.clone(),
            created_at_ms: self.now,
            artifact: artifact.clone(),
        })
    }

    fn insert_dream_run(&mut self, row: &DreamRunRow) -> Result<(), StorageError> {
        let current = state(self.conn, &row.scope)?;
        if current.revision != row.settings_revision {
            return Err(revision_conflict(
                "dreaming_settings",
                &row.scope.key(),
                row.settings_revision,
                current.revision,
            ));
        }
        row.policy.validate()?;
        if row.revision != 1
            || row.generation != 1
            || !row.cutoffs.does_not_precede(&DreamCutoffs::default())
            || !matches!(
                row.phase,
                DreamPhase::Queued
                    | DreamPhase::Disabled
                    | DreamPhase::TooSoon
                    | DreamPhase::Busy
                    | DreamPhase::NoSignal
                    | DreamPhase::Blocked
                    | DreamPhase::ScheduledFanout
            )
            || row.phase.ended() != row.ended_at_ms.is_some()
            || (row.phase.ended() && row.report.is_none())
        {
            return Err(invalid("invalid initial Dreaming run"));
        }
        if row.phase.owns_scope() && live_run(self.conn, &row.scope)?.is_some() {
            return Err(StorageError::Busy);
        }
        if let Some(parent) = &row.continued_from {
            let previous = run(self.conn, parent)?.ok_or_else(|| not_found(parent))?;
            let already_continued: bool = db(self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM dreaming_runs WHERE continued_from=?1 AND operation_id IS NOT NULL)",
                [parent.as_str()], |row| row.get(0),
            ))?;
            if row.trigger != DreamTrigger::Resume
                || row.scope != previous.scope
                || !previous.phase.resumable()
                || previous.cutoffs != row.cutoffs
                || previous.checkpoint.is_none()
                || already_continued
            {
                return Err(invalid(
                    "Dreaming continuation no longer owns its frozen scope",
                ));
            }
        } else if row.trigger == DreamTrigger::Resume {
            return Err(invalid("Dreaming resume requires a saved run"));
        } else {
            validate_checkpoint(None, row.checkpoint.as_ref())?;
        }
        db(self.conn.execute(
            "INSERT OR IGNORE INTO dreaming_scope_state(scope_key,project_id,settings_revision,settings_json)
             VALUES(?1,?2,?3,?4)",
            params![row.scope.key(), row.scope.project().map(|id| id.0.as_str()),
                current.revision, encode(&current.policy)?],
        ))?;
        db(self.conn.execute(
            "INSERT INTO dreaming_runs(id,scope_key,project_id,continued_from,operation_id,
                 initiator_id,trigger,revision,generation,state,settings_revision,settings_json,
                 cutoffs_json,started_at,ended_at,checkpoint_json,report_json)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![
                row.id.as_str(),
                row.scope.key(),
                row.scope.project().map(|id| id.0.as_str()),
                row.continued_from.as_ref().map(Id::as_str),
                row.operation_id.as_ref().map(|id| id.0.as_str()),
                row.initiator.0.as_str(),
                token(&row.trigger)?,
                row.revision,
                row.generation,
                token(&row.phase)?,
                row.settings_revision,
                encode(&row.policy)?,
                encode(&row.cutoffs)?,
                row.started_at_ms,
                row.ended_at_ms,
                row.checkpoint.as_ref().map(encode).transpose()?,
                row.report.as_ref().map(encode).transpose()?
            ],
        ))?;
        Ok(())
    }

    fn advance_dream_run(
        &mut self,
        id: &Id,
        expected_revision: i64,
        generation: i64,
        progress: &DreamProgress,
    ) -> Result<DreamRunRow, StorageError> {
        let old = run(self.conn, id)?.ok_or_else(|| not_found(id))?;
        if old.generation != generation {
            return Err(lease_lost(id));
        }
        if old.revision != expected_revision {
            return Err(revision_conflict(
                "dreaming_run",
                id.as_str(),
                expected_revision,
                old.revision,
            ));
        }
        validate_progress(&old, progress)?;
        let completed = progress.phase == DreamPhase::Completed;
        let ended_at = progress
            .phase
            .ended()
            .then_some(self.now.max(old.started_at_ms));
        let checkpoint = if completed {
            None
        } else {
            progress.checkpoint.as_ref().map(encode).transpose()?
        };
        if completed {
            let current = state(self.conn, &old.scope)?;
            if current
                .completed
                .is_some_and(|(cutoffs, _)| !old.cutoffs.does_not_precede(&cutoffs))
            {
                return Err(invalid("Dreaming run cutoffs precede the completed sweep"));
            }
        }
        let revision = next_revision(expected_revision)?;
        let generation = if progress.phase.owns_scope() {
            old.generation
        } else {
            next_revision(old.generation)?
        };
        db(self.conn.execute(
            "UPDATE dreaming_runs SET revision=?2,generation=?3,state=?4,ended_at=?5,checkpoint_json=?6,report_json=?7 WHERE id=?1",
            params![id.as_str(), revision, generation, token(&progress.phase)?, ended_at, checkpoint,
                progress.report.as_ref().map(encode).transpose()?],
        ))?;
        run(self.conn, id)?.ok_or_else(|| corrupt("updated Dreaming run disappeared"))
    }

    fn put_dream_action(
        &mut self,
        id: &Id,
        generation: i64,
        action: &Id,
        outcome: &Value,
    ) -> Result<DreamActionRow, StorageError> {
        let run = run(self.conn, id)?.ok_or_else(|| not_found(id))?;
        if run.phase != DreamPhase::Running || run.generation != generation {
            return Err(lease_lost(id));
        }
        if let Some(saved) = self.dream_action(id, action)? {
            if saved.action == *outcome {
                return Ok(saved);
            }
            return Err(StorageError::Duplicate {
                kind: "dreaming_action",
                id: action.as_str().to_owned(),
            });
        }
        db(self.conn.execute(
            "INSERT INTO dreaming_actions(run_id,action_id,position,committed_at,action_json)
             SELECT ?1,?2,coalesce(max(position),0)+1,?3,?4 FROM dreaming_actions WHERE run_id=?1",
            params![id.as_str(), action.as_str(), self.now, encode(outcome)?],
        ))?;
        self.dream_action(id, action)?
            .ok_or_else(|| corrupt("committed Dreaming action disappeared"))
    }
}
