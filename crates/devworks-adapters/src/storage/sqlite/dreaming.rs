//! SQLite transactions fence Dreaming progress and advance a completed cutoff with its report.
use super::conn::db;
use super::repos::{Snapshot, Unit};
use super::support::{conflict, corrupt, id_of, invalid, page_limit};
use devworks_core::ports::storage::{
    DreamActionRow, DreamCheckpoint, DreamClaimRef, DreamClaimWindow, DreamCutoffs,
    DreamInstruction, DreamPhase, DreamPolicy, DreamProgress, DreamRequestStage, DreamRunRow,
    DreamScopeState, DreamSignal, DreamStream, DreamTrailPosition, DreamTrailQuery, DreamTrailRow,
    DreamTrigger, DreamingRead, DreamingRepository, KnowledgeScope, StorageError,
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
        DreamStream::Reviews | DreamStream::UnlearnedReviews => {
            "SELECT max(
                (SELECT coalesce(max(seq),0) FROM events WHERE project_id IS ?1 AND kind='PlanReviewSettled'),
                (SELECT coalesce(max(seq),0) FROM history_links WHERE project_id=?1 AND kind='review'))"
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

fn trail(
    conn: &Connection,
    query: &DreamTrailQuery<'_>,
) -> Result<Vec<DreamTrailRow>, StorageError> {
    let project = query.scope.project().map(|id| id.0.as_str());
    let mut statement = db(conn.prepare_cached(
        "WITH trail(kind,id,item_id,at,content) AS (
            SELECT 'revision',h.target_id || ':' || h.revision,NULL,h.at,h.snapshot_json
              FROM events e JOIN knowledge_history h ON h.write_id=e.aggregate_id
             WHERE e.project_id IS ?1 AND e.kind='KnowledgeWritten' AND e.seq<=?2
               AND h.target_kind='Claim' AND h.lifecycle<>'Deleted' AND (?5 IS NULL OR ?5='revision')
            UNION ALL
            SELECT 'item',w.id,w.id,coalesce(d.updated_at,w.created_at),
                json_object('title',w.title,'revision',w.revision,'brief',json(w.brief_json))
              FROM work_items w LEFT JOIN work_details d ON d.work_id=w.id
             WHERE w.project_id=?1 AND ?5='item'
            UNION ALL
            SELECT 'attempt',json_extract(a.value,'$.id'),w.id,coalesce(d.updated_at,w.created_at),a.value
              FROM work_items w LEFT JOIN work_details d ON d.work_id=w.id
                JOIN json_each(w.brief_json,'$.attempts') a
             WHERE w.project_id=?1 AND (?5 IS NULL OR ?5='attempt')
            UNION ALL
            SELECT 'handoff',json_extract(h.value,'$.id'),w.id,coalesce(d.updated_at,w.created_at),h.value
              FROM work_items w LEFT JOIN work_details d ON d.work_id=w.id
                JOIN json_each(w.brief_json,'$.handoffs') h
             WHERE w.project_id=?1 AND (?5 IS NULL OR ?5='handoff')
            UNION ALL
            SELECT 'thread',json_extract(c.value,'$.id'),w.id,coalesce(d.updated_at,w.created_at),c.value
              FROM work_items w LEFT JOIN work_details d ON d.work_id=w.id
                JOIN json_each(w.brief_json,'$.comments') c
             WHERE w.project_id=?1 AND (?5 IS NULL OR ?5='thread')
            UNION ALL
            SELECT 'message',m.id,coalesce(m.target_item_id,m.root_id),coalesce(m.resolved_at,m.created_at),
                json_object('body',m.body,'kind',m.message_class,'author',m.from_actor_id,
                    'state',m.state,'revision',m.revision)
              FROM agent_messages m
             WHERE m.project_id=?1 AND m.channel<>'Session' AND (?5 IS NULL OR ?5='message')
            UNION ALL
            SELECT 'review',e.id,e.aggregate_id,e.created_at,e.payload_json
              FROM events e WHERE e.project_id IS ?1 AND e.kind='PlanReviewSettled' AND e.seq<=?3
               AND (?5 IS NULL OR ?5='review')
            UNION ALL
            SELECT 'review',l.id,l.root_id,l.created_at,
                json_object('kind',l.kind,'summary',l.summary,'author',l.actor_id,'evidence',json(l.body_json))
              FROM history_links l
             WHERE l.project_id=?1 AND l.kind='review' AND l.seq<=?3 AND (?5 IS NULL OR ?5='review')
        ) SELECT kind,id,item_id,at,content FROM trail
           WHERE id IS NOT NULL AND (at<=?4 OR (?6 IS NOT NULL AND id=?6))
             AND (?6 IS NULL OR id=?6) AND (?7 IS NULL OR item_id=?7)
             AND (?8 IS NULL OR at>=?8)
             AND (?9 IS NULL OR at<?9 OR (at=?9 AND (id>?10 OR (id=?10 AND kind>?11))))
           ORDER BY at DESC,id,kind LIMIT ?12 OFFSET ?13"
    ))?;
    let offset = i64::try_from(query.offset).map_err(|_| invalid("invalid work trail offset"))?;
    let limit = page_limit(query.limit)?;
    let after = query.before;
    db(statement.query_map(
        params![
            project,
            query.knowledge_through,
            query.reviews_through,
            query.through_ms,
            query.kind,
            query.id,
            query.item_id,
            query.since_ms,
            after.map(|row| row.at_ms),
            after.map(|row| row.id.as_str()),
            after.map(|row| row.kind.as_str()),
            limit,
            offset,
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            ))
        },
    ))?
    .map(|row| {
        let (kind, id, item_id, at_ms, content) = db(row)?;
        Ok(DreamTrailRow {
            position: DreamTrailPosition { at_ms, id, kind },
            item_id,
            content: decode(&content)?,
        })
    })
    .collect()
}

macro_rules! dreaming_reads {
    ($reader:ty) => {
        impl DreamingRead for $reader {
            fn dream_trail(&self, query: &DreamTrailQuery<'_>) -> Result<Vec<DreamTrailRow>, StorageError> {
                trail(self.conn, query)
            }
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
                        AND kind='PlanReviewSettled' AND seq>?2 AND seq<=?3
                        UNION ALL SELECT seq,id,created_at,NULL FROM history_links
                        WHERE project_id=?1 AND kind='review' AND seq>?2 AND seq<=?3
                        ORDER BY seq LIMIT ?4", project),
                    DreamStream::UnlearnedReviews => ("SELECT review.seq,review.id,review.at,NULL FROM (
                        SELECT e.seq,e.id,e.created_at AS at,NULL AS ref FROM events e WHERE e.project_id IS ?1
                          AND e.kind='PlanReviewSettled' AND e.seq>?2 AND e.seq<=?3
                        UNION ALL SELECT l.seq,l.id,l.created_at,json_extract(l.body_json,'$.url') FROM history_links l
                         WHERE l.project_id=?1 AND l.kind='review' AND l.seq>?2 AND l.seq<=?3
                        ) review WHERE NOT EXISTS(
                            SELECT 1 FROM knowledge_lesson_events l JOIN knowledge_claims c ON c.id=l.claim_id
                            WHERE c.project_id IS ?1 AND c.lifecycle='Active'
                              AND (l.source_ref=review.id OR l.source_ref=review.ref))
                        ORDER BY review.seq LIMIT ?4", project),
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
                            AND frozen.revision=CASE WHEN ?4 IS NOT NULL THEN claim.revision ELSE (
                                SELECT h.revision FROM knowledge_history h
                                 JOIN events e ON e.aggregate_kind='knowledge_write'
                                    AND e.aggregate_id=h.write_id AND e.kind='KnowledgeWritten'
                                 WHERE h.target_id=claim.id AND e.seq<=?2
                                    AND e.project_id IS ?7
                                 ORDER BY h.revision DESC LIMIT 1) END
                      WHERE claim.scope_key=?1 AND claim.id>?3 AND claim.lifecycle<>'Deleted'
                        AND frozen.lifecycle<>'Deleted'
                        AND (?4 IS NULL OR claim.id=?4)
                        AND (?5 IS NULL OR coalesce(json_extract(frozen.snapshot_json,'$.entryKind'),
                            json_extract(frozen.snapshot_json,'$.kind'))=?5)
                        AND (?6 IS NULL OR frozen.lifecycle=?6)
                        AND (?9 IS NULL OR coalesce(json_extract(frozen.snapshot_json,'$.stale'),0)=?9)
                        AND (?10 IS NULL OR (
                          frozen.lifecycle='Active' AND (
                            (?10='recurrence' AND cast(json_extract(frozen.snapshot_json,'$.lesson.recurrence') AS INTEGER)>1)
                            OR (?10 IN('unaddressed','unmatched')
                              AND json_extract(frozen.snapshot_json,'$.lesson.ruleState')='Effective'
                              AND coalesce(json_array_length(frozen.snapshot_json,'$.lesson.addressedBy'),0)=0
                              AND (?10='unaddressed' OR NOT EXISTS(
                                SELECT 1 FROM knowledge_deliveries d WHERE d.claim_id=claim.id
                                  AND d.delivered_at>=?11 AND d.delivered_at<=?12))))))
                      ORDER BY claim.id LIMIT ?8",
                ))?;
                let mut rows = db(statement.query(params![
                    window.scope.key(),window.through,window.after.map_or("",Id::as_str),
                    window.exact.map(Id::as_str),window.kind,window.status,
                    window.scope.project().map(|project| project.0.as_str()),page_limit(window.limit)?, window.possibly_stale, window.lessons,
                    window.delivered_since_ms, window.captured_at_ms,
                ]))?;
                let mut result = Vec::new();
                while let Some(row) = db(rows.next())? {
                    result.push(DreamClaimRef { id:id_of(db(row.get(0))?)?,
                        revision:db(row.get(1))?,current_revision:db(row.get(2))?,
                        lifecycle:db(row.get(3))?,entry_kind:db(row.get(4))? });
                }
                Ok(result)
            }

            fn dream_prior_proposal(
                &self, scope: &KnowledgeScope, target: &Id, action: &str,
            ) -> Result<Option<Id>, StorageError> {
                db(self.conn.query_row(
                    "SELECT id FROM decision_approvals
                      WHERE scope_key=?1 AND target_id=?2 AND operation_kind='dreaming.propose'
                        AND json_extract(payload_json,'$.reviewed.change.kind')=?3
                        AND state IN ('Pending','Rejected')
                      ORDER BY (state='Pending') DESC, coalesce(decided_at,created_at) DESC,
                               created_at DESC, id DESC LIMIT 1",
                    params![scope.key(), target.as_str(), action],
                    |row| row.get::<_, String>(0),
                ).optional())?.map(id_of).transpose()
            }

            fn dream_lesson_recurred_while_served(&self, claim: &Id) -> Result<bool, StorageError> {
                db(self.conn.query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM knowledge_lesson_events e
                       JOIN knowledge_deliveries d
                         ON d.claim_id=e.claim_id AND d.session_id=e.session_id AND d.delivered_at<=e.at
                       JOIN knowledge_history h ON h.target_id=d.claim_id AND h.revision=d.revision
                       WHERE e.claim_id=?1 AND json_extract(h.snapshot_json,'$.lesson.ruleState')='Effective'
                         AND EXISTS (SELECT 1 FROM knowledge_lesson_events first
                                      WHERE first.claim_id=e.claim_id AND first.at<e.at)
                     )",
                    [claim.as_str()], |row| row.get(0),
                ))
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

            fn dream_resumable_run(&self, scope: &KnowledgeScope) -> Result<Option<DreamRunRow>, StorageError> {
                let id = db(self.conn.query_row(
                    "SELECT r.id FROM dreaming_runs r WHERE r.scope_key=?1
                       AND r.state IN ('Blocked','BudgetExhausted','Partial','Cancelled')
                       AND r.checkpoint_json IS NOT NULL
                       AND (json_type(r.checkpoint_json,'$.continuation')='object'
                            OR json_type(r.checkpoint_json,'$.request')='object')
                       AND NOT EXISTS (SELECT 1 FROM dreaming_runs child
                           WHERE child.continued_from=r.id AND child.operation_id IS NOT NULL)
                       ORDER BY r.started_at DESC,r.id DESC LIMIT 1",
                    [scope.key()], |row| row.get::<_,String>(0),
                ).optional())?;
                match id { Some(id) => run(self.conn, &id_of(id)?), None => Ok(None) }
            }

            fn dream_continuation_used(&self, run: &Id) -> Result<bool, StorageError> {
                db(self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM dreaming_runs
                                   WHERE continued_from=?1 AND operation_id IS NOT NULL)",
                    [run.as_str()], |row| row.get(0),
                ))
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
        && ((request.stage != DreamRequestStage::Observed && request.result.is_some())
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
            let expected_cutoffs = DreamCutoffs {
                sweep_started_at_ms: previous.sweep_started_at_ms(),
                base_instruction_revision: instruction_tip(self.conn, &KnowledgeScope::Workspace)?,
                project_instruction_revision: if row.scope.project().is_some() {
                    instruction_tip(self.conn, &row.scope)?
                } else {
                    0
                },
                ..previous.cutoffs.clone()
            };
            if row.trigger != DreamTrigger::Resume
                || row.scope != previous.scope
                || !previous.phase.resumable()
                || expected_cutoffs != row.cutoffs
                || previous.checkpoint.is_none()
                || self.dream_continuation_used(parent)?
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
        // All actions in a continuation keep their original ordinal namespace.
        let latest: Option<i64> = db(self.conn.query_row(
            "SELECT max(position) FROM dreaming_actions WHERE run_id=?1",
            [id.as_str()],
            |row| row.get(0),
        ))?;
        let latest = match (latest, run.continued_from) {
            (Some(latest), _) => latest,
            (None, Some(parent)) => db(self.conn.query_row(
                "WITH RECURSIVE ancestors(id,continued_from) AS (
                   SELECT id,continued_from FROM dreaming_runs WHERE id=?1
                   UNION ALL SELECT p.id,p.continued_from FROM dreaming_runs p JOIN ancestors a ON p.id=a.continued_from
                 ) SELECT coalesce(max(position),0) FROM dreaming_actions JOIN ancestors ON ancestors.id=run_id",
                [parent.as_str()], |row| row.get(0),
            ))?,
            (None, None) => 0,
        };
        let position = latest
            .checked_add(1)
            .ok_or_else(|| invalid("Dreaming action position is exhausted"))?;
        db(self.conn.execute(
            "INSERT INTO dreaming_actions(run_id,action_id,position,committed_at,action_json)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                id.as_str(),
                action.as_str(),
                position,
                self.now,
                encode(outcome)?
            ],
        ))?;
        self.dream_action(id, action)?
            .ok_or_else(|| corrupt("committed Dreaming action disappeared"))
    }
}
