//! SQLite session, transcript and lineage records: `sessions`/`conversations` and
//! `transcript_chunks` (migration 1, extended by migration 7), and the migration 7 tables
//! `transcript_batches`, `session_tool_calls`, `session_context_epochs`,
//! `session_context_sources`, `session_checkpoints`, `session_usage` and `history_links`.
use super::conn::db;
use super::repos::{EVENT_COLUMNS, Snapshot, Unit, event_of, event_row};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{ActorId, Id, OperationId, ProjectId, RootId};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

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
fn json_of(s: &str) -> Result<Value, StorageError> {
    serde_json::from_str(s).map_err(|e| corrupt(format!("stored JSON: {e}")))
}
fn bounded(limit: u16) -> Result<i64, StorageError> {
    if limit == 0 || limit > MAX_PAGE {
        return Err(invalid("listing limit must be 1..=500"));
    }
    Ok(limit as i64)
}
fn text(field: &str, s: &str, max: usize) -> Result<(), StorageError> {
    if s.is_empty() || s.len() > max || s.contains('\0') {
        return Err(invalid(format!("{field} must be 1..={max} bytes")));
    }
    Ok(())
}
fn exists<P: rusqlite::Params>(conn: &Connection, sql: &str, p: P) -> Result<bool, StorageError> {
    db(db(conn.prepare_cached(sql))?.exists(p))
}
fn require<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    p: P,
    kind: &'static str,
    id: &str,
) -> Result<(), StorageError> {
    if exists(conn, sql, p)? {
        Ok(())
    } else {
        Err(StorageError::NotFound {
            kind,
            id: id.to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Reads

fn work_root(conn: &Connection, root: &RootId) -> Result<Option<WorkRoot>, StorageError> {
    let raw = db(db(conn.prepare_cached(
        "SELECT id,project_id,kind,title,revision,scope_revision,parent_feature_id,created_at FROM work_items WHERE id=?1",
    ))?
    .query_row([root.0.as_str()], work_row)
    .optional())?;
    raw.map(work_of).transpose()
}
type WorkRaw = (
    String,
    String,
    String,
    String,
    i64,
    i64,
    Option<String>,
    i64,
);
fn work_row(r: &Row<'_>) -> rusqlite::Result<WorkRaw> {
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
}
fn work_of(raw: WorkRaw) -> Result<WorkRoot, StorageError> {
    let (id, project, kind, title, revision, scope, parent, created) = raw;
    Ok(WorkRoot {
        id: RootId(id_of(id)?),
        project_id: ProjectId(id_of(project)?),
        kind,
        title,
        revision,
        scope_revision: scope,
        parent_feature_id: opt_id(parent)?.map(RootId),
        created_at_ms: created,
    })
}

const SESSION_COLUMNS: &str = "s.id,s.conversation_id,s.agent_id,s.project_id,s.root_id,s.title,s.context_epoch,s.revision,s.created_by,s.created_at,s.closed_at,s.closed_by,s.next_seq,s.checkpoint_seq";
type SessionRaw = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    i64,
    i64,
    Option<String>,
    i64,
    Option<i64>,
    Option<String>,
    i64,
    i64,
);
fn session_row(r: &Row<'_>) -> rusqlite::Result<SessionRaw> {
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
    ))
}
fn session_of(raw: SessionRaw) -> Result<StoredSession, StorageError> {
    let (
        id,
        conversation,
        agent,
        project,
        root,
        title,
        epoch,
        revision,
        by,
        at,
        closed_at,
        closed_by,
        next,
        checkpoint,
    ) = raw;
    let agent = ActorId(id_of(agent)?);
    Ok(StoredSession {
        id: id_of(id)?,
        conversation_id: id_of(conversation)?,
        // A session recorded before migration 7 has no creator; its agent created it.
        created_by: opt_id(by)?.map_or_else(|| agent.clone(), ActorId),
        agent_id: agent,
        project_id: opt_id(project)?.map(ProjectId),
        root_id: opt_id(root)?.map(RootId),
        title,
        context_epoch: epoch,
        revision,
        created_at_ms: at,
        closed_at_ms: closed_at,
        closed_by: opt_id(closed_by)?.map(ActorId),
        next_sequence: next,
        checkpoint_sequence: checkpoint,
    })
}
fn session(conn: &Connection, id: &Id) -> Result<Option<StoredSession>, StorageError> {
    let raw = db(db(conn.prepare_cached(&format!(
        "SELECT {SESSION_COLUMNS} FROM sessions s WHERE s.id=?1"
    )))?
    .query_row([id.as_str()], session_row)
    .optional())?;
    raw.map(session_of).transpose()
}
fn sessions(
    conn: &Connection,
    scope: &SessionScope,
    closed: Option<bool>,
    after: Option<&SessionPosition>,
    limit: u16,
) -> Result<Vec<StoredSession>, StorageError> {
    let n = bounded(limit)?;
    let (scope_kind, project) = match scope {
        SessionScope::All => (0, None),
        SessionScope::Project(p) => (1, Some(p.0.as_str())),
        SessionScope::Workspace => (2, None),
    };
    let closed_kind = match closed {
        None => 0,
        Some(true) => 1,
        Some(false) => 2,
    };
    let mut stmt = db(conn.prepare_cached(&format!(
        "SELECT {SESSION_COLUMNS} FROM sessions s
         WHERE (?5=0 OR (?5=1 AND s.project_id=?1) OR (?5=2 AND s.project_id IS NULL))
         AND (?6=0 OR (?6=1 AND s.closed_at IS NOT NULL) OR (?6=2 AND s.closed_at IS NULL))
         AND (?2 IS NULL OR s.created_at<?2 OR (s.created_at=?2 AND s.id<?3))
         ORDER BY s.created_at DESC,s.id DESC LIMIT ?4"
    )))?;
    let rows = db(stmt.query_map(
        params![
            project,
            after.map(|a| a.created_at_ms),
            after.map(|a| a.id.as_str().to_owned()),
            n,
            scope_kind,
            closed_kind
        ],
        session_row,
    ))?;
    db(rows.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(session_of)
        .collect()
}

const ENTRY_COLUMNS: &str =
    "session_id,seq,entry_id,batch_id,context_epoch,actor_id,kind,created_at,payload_json";
type EntryRaw = (
    String,
    i64,
    Option<String>,
    Option<String>,
    i64,
    Option<String>,
    String,
    i64,
    String,
);
fn entry_row(r: &Row<'_>) -> rusqlite::Result<EntryRaw> {
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
    ))
}
fn entry_of(raw: EntryRaw) -> Result<StoredEntry, StorageError> {
    let (session, seq, entry, batch, epoch, actor, kind, at, payload) = raw;
    let missing = || corrupt("transcript entry without identity or attribution");
    Ok(StoredEntry {
        session_id: id_of(session)?,
        sequence: seq,
        entry_id: id_of(entry.ok_or_else(missing)?)?,
        batch_id: id_of(batch.ok_or_else(missing)?)?,
        context_epoch: epoch,
        actor: ActorId(id_of(actor.ok_or_else(missing)?)?),
        kind,
        created_at_ms: at,
        body: json_of(&payload)?,
    })
}

const BATCH_COLUMNS: &str = "batch_id,session_id,payload_digest,actor_id,context_epoch,first_seq,last_seq,event_id,committed_seq,accepted_at";
fn batch(conn: &Connection, session: &Id, id: &Id) -> Result<Option<StoredBatch>, StorageError> {
    type Raw = (
        String,
        String,
        String,
        String,
        i64,
        i64,
        i64,
        String,
        i64,
        i64,
    );
    let raw: Option<Raw> = db(db(conn.prepare_cached(&format!(
        "SELECT {BATCH_COLUMNS} FROM transcript_batches WHERE session_id=?1 AND batch_id=?2"
    )))?
    .query_row([session.as_str(), id.as_str()], |r| {
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
        ))
    })
    .optional())?;
    raw.map(|(b, s, d, a, e, f, l, ev, c, at)| {
        Ok(StoredBatch {
            batch_id: id_of(b)?,
            session_id: id_of(s)?,
            payload_digest: d,
            actor: ActorId(id_of(a)?),
            context_epoch: e,
            first_sequence: f,
            last_sequence: l,
            event_id: id_of(ev)?,
            committed_sequence: c,
            accepted_at_ms: at,
        })
    })
    .transpose()
}

const CALL_COLUMNS: &str = "session_id,attempt_id,native_call_id,ordinal,capability_id,call_seq,response_seq,outcome,operation_id";
type CallRaw = (
    String,
    String,
    String,
    i64,
    String,
    i64,
    Option<i64>,
    Option<String>,
    Option<String>,
);
fn call_row(r: &Row<'_>) -> rusqlite::Result<CallRaw> {
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
    ))
}
fn call_of(raw: CallRaw) -> Result<StoredToolCall, StorageError> {
    let (session, attempt, native, ordinal, capability, call, response, outcome, op) = raw;
    Ok(StoredToolCall {
        session_id: id_of(session)?,
        attempt_id: id_of(attempt)?,
        native_call_id: native,
        ordinal,
        capability_id: capability,
        call_sequence: call,
        response_sequence: response,
        outcome,
        operation_id: opt_id(op)?.map(OperationId),
    })
}
fn calls(
    conn: &Connection,
    sql_filter: &str,
    session: &Id,
    limit: u16,
) -> Result<Vec<StoredToolCall>, StorageError> {
    let n = bounded(limit)?;
    let mut stmt = db(conn.prepare_cached(&format!(
        "SELECT {CALL_COLUMNS} FROM session_tool_calls WHERE session_id=?1 AND {sql_filter} LIMIT ?2"
    )))?;
    let rows = db(stmt.query_map(params![session.as_str(), n], call_row))?;
    db(rows.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(call_of)
        .collect()
}

fn epoch(conn: &Connection, session: &Id, epoch: i64) -> Result<Option<StoredEpoch>, StorageError> {
    type Raw = (String, i64, i64, String, String, String, i64, i64);
    let raw: Option<Raw> = db(db(conn.prepare_cached(
        "SELECT session_id,epoch,previous_epoch,reason,context_json,identity,seq,created_at FROM session_context_epochs WHERE session_id=?1 AND epoch=?2",
    ))?
    .query_row(params![session.as_str(), epoch], |r| {
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
    .optional())?;
    raw.map(|(s, e, p, reason, ctx, key, seq, at)| {
        Ok(StoredEpoch {
            session_id: id_of(s)?,
            epoch: e,
            previous_epoch: p,
            reason,
            context: json_of(&ctx)?,
            identity: key,
            sequence: seq,
            created_at_ms: at,
        })
    })
    .transpose()
}

fn context_sources(
    conn: &Connection,
    session: &Id,
    epoch: i64,
    after: i64,
    limit: u16,
) -> Result<Vec<StoredContextSource>, StorageError> {
    let n = bounded(limit)?;
    type Raw = (
        String,
        i64,
        i64,
        String,
        String,
        i64,
        Option<String>,
        i64,
        String,
        i64,
    );
    let mut stmt = db(conn.prepare_cached(
        "SELECT session_id,epoch,ordinal,source_id,kind,source_revision,artifact_json,token_estimate,disposition,seq FROM session_context_sources WHERE session_id=?1 AND epoch=?2 AND ordinal>?3 ORDER BY ordinal LIMIT ?4",
    ))?;
    let rows = db(
        stmt.query_map(params![session.as_str(), epoch, after, n], |r| {
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
            ))
        }),
    )?;
    let raw: Vec<Raw> = db(rows.collect())?;
    raw.into_iter()
        .map(|(s, e, o, src, kind, rev, art, tokens, disp, seq)| {
            Ok(StoredContextSource {
                session_id: id_of(s)?,
                epoch: e,
                ordinal: o,
                source_id: id_of(src)?,
                kind,
                source_revision: rev,
                artifact: art.as_deref().map(json_of).transpose()?,
                token_estimate: tokens,
                disposition: disp,
                sequence: seq,
            })
        })
        .collect()
}

fn provider_context_head(
    conn: &Connection,
    session: &Id,
) -> Result<Option<StoredProviderContextHead>, StorageError> {
    type Raw = (i64, i64, String, Option<String>, String, String, i64);
    let raw: Option<Raw> = db(conn.query_row(
        "SELECT context_epoch,observed_sequence,kind,attempt_id,binding_json,artifact_json,revision FROM session_provider_context_heads WHERE session_id=?1",
        [session.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
    ).optional())?;
    raw.map(
        |(context_epoch, observed_sequence, kind, attempt_id, binding, artifact, revision)| {
            Ok(StoredProviderContextHead {
                session_id: session.clone(),
                context_epoch,
                observed_sequence,
                kind,
                attempt_id: attempt_id.map(id_of).transpose()?,
                binding: json_of(&binding)?,
                artifact: json_of(&artifact)?,
                revision,
            })
        },
    )
    .transpose()
}

fn latest_checkpoint(
    conn: &Connection,
    session: &Id,
) -> Result<Option<StoredCheckpoint>, StorageError> {
    let raw: Option<(String, i64, String, String, i64)> = db(db(conn.prepare_cached(
        "SELECT session_id,seq,batch_id,artifact_json,created_at FROM session_checkpoints WHERE session_id=?1 ORDER BY seq DESC LIMIT 1",
    ))?
    .query_row([session.as_str()], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .optional())?;
    raw.map(|(s, seq, b, art, at)| {
        Ok(StoredCheckpoint {
            session_id: id_of(s)?,
            sequence: seq,
            batch_id: id_of(b)?,
            artifact: json_of(&art)?,
            created_at_ms: at,
        })
    })
    .transpose()
}

fn usage(
    conn: &Connection,
    root: &RootId,
    after: Option<&SequenceKey>,
    limit: u16,
) -> Result<Vec<(SequenceKey, StoredUsage)>, StorageError> {
    let n = bounded(limit)?;
    // One batch append is one committed sequence, so (committed sequence, ordinal) orders the
    // observations; `id` is the zero-padded ordinal so it compares in that order.
    let (seq, id) = after.map_or((-1, String::new()), |k| (k.sequence, k.id.clone()));
    type Raw = (
        String,
        i64,
        String,
        Option<String>,
        Option<String>,
        i64,
        String,
    );
    let mut stmt = db(conn.prepare_cached(
        "SELECT batch_id,ordinal,session_id,project_id,root_id,committed_seq,item_json FROM session_usage
         WHERE root_id=?1 AND (committed_seq>?2 OR (committed_seq=?2 AND printf('%06d',ordinal)>?3))
         ORDER BY committed_seq,ordinal LIMIT ?4",
    ))?;
    let rows = db(stmt.query_map(params![root.0.as_str(), seq, id, n], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
        ))
    }))?;
    let raw: Vec<Raw> = db(rows.collect())?;
    raw.into_iter()
        .map(|(b, o, s, p, r, c, item)| {
            let key = SequenceKey {
                sequence: c,
                source: 3,
                id: format!("{o:06}"),
            };
            Ok((
                key,
                StoredUsage {
                    session_id: id_of(s)?,
                    batch_id: id_of(b)?,
                    ordinal: o,
                    project_id: opt_id(p)?.map(ProjectId),
                    root_id: opt_id(r)?.map(RootId),
                    committed_sequence: c,
                    item: json_of(&item)?,
                },
            ))
        })
        .collect()
}

const LINK_COLUMNS: &str = "id,project_id,root_id,kind,target_kind,target_id,parent_id,supersedes_id,actor_id,event_id,seq,repository_id,revision,summary,body_json,created_at";
type LinkRaw = (
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    i64,
    Option<String>,
    Option<String>,
    String,
    String,
    i64,
);
fn link_row(r: &Row<'_>) -> rusqlite::Result<LinkRaw> {
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
        r.get(15)?,
    ))
}
fn link_of(raw: LinkRaw) -> Result<StoredLink, StorageError> {
    let (
        id,
        project,
        root,
        kind,
        tkind,
        tid,
        parent,
        supersedes,
        actor,
        event,
        seq,
        repo,
        rev,
        summary,
        body,
        at,
    ) = raw;
    Ok(StoredLink {
        id: id_of(id)?,
        project_id: ProjectId(id_of(project)?),
        root_id: RootId(id_of(root)?),
        kind,
        target_kind: tkind,
        target_id: tid,
        parent_id: opt_id(parent)?,
        supersedes_id: opt_id(supersedes)?,
        actor: ActorId(id_of(actor)?),
        event_id: id_of(event)?,
        sequence: seq,
        repository_id: opt_id(repo)?,
        revision: rev,
        summary,
        body: json_of(&body)?,
        created_at_ms: at,
    })
}
fn link(conn: &Connection, id: &Id) -> Result<Option<StoredLink>, StorageError> {
    let raw = db(db(conn.prepare_cached(&format!(
        "SELECT {LINK_COLUMNS} FROM history_links WHERE id=?1"
    )))?
    .query_row([id.as_str()], link_row)
    .optional())?;
    raw.map(link_of).transpose()
}
fn kinds_json(kinds: &[&str]) -> String {
    serde_json::to_string(kinds).unwrap_or_else(|_| "[]".into())
}
fn links(
    conn: &Connection,
    root: &RootId,
    kinds: &[&str],
    after: Option<&SequenceKey>,
    limit: u16,
) -> Result<Vec<StoredLink>, StorageError> {
    let n = bounded(limit)?;
    let (seq, id) = after.map_or((-1, String::new()), |k| (k.sequence, k.id.clone()));
    let mut stmt = db(conn.prepare_cached(&format!(
        "SELECT {LINK_COLUMNS} FROM history_links WHERE root_id=?1
         AND kind IN (SELECT value FROM json_each(?2))
         AND (seq>?3 OR (seq=?3 AND id>?4)) ORDER BY seq,id LIMIT ?5"
    )))?;
    let rows = db(stmt.query_map(
        params![root.0.as_str(), kinds_json(kinds), seq, id, n],
        link_row,
    ))?;
    db(rows.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(link_of)
        .collect()
}
fn links_to(
    conn: &Connection,
    target_kind: &str,
    target_id: &str,
    limit: u16,
) -> Result<Vec<StoredLink>, StorageError> {
    let n = bounded(limit)?;
    let mut stmt = db(conn.prepare_cached(&format!(
        "SELECT {LINK_COLUMNS} FROM history_links WHERE target_kind=?1 AND target_id=?2 ORDER BY seq,id LIMIT ?3"
    )))?;
    let rows = db(stmt.query_map(params![target_kind, target_id, n], link_row))?;
    db(rows.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(link_of)
        .collect()
}

/// Event kinds of the history record that belong to a root's lineage besides its links.
const LINEAGE_EVENT_AGGREGATES: &str = "'command','artifact'";

fn lineage(
    conn: &Connection,
    root: &RootId,
    after: Option<&SequenceKey>,
    limit: u16,
) -> Result<Vec<LineageRow>, StorageError> {
    let n = bounded(limit)?;
    let key = after.cloned().unwrap_or(SequenceKey {
        sequence: -1,
        source: 0,
        id: String::new(),
    });
    // One keyset over three sources: the work hierarchy, the root's links and its command and
    // artifact history facts, ordered by (sequence, source, id). Related work has the fixed key
    // (0, 0, id), which never changes once visible; a change to the hierarchy changes the
    // membership digest that lineage cursors bind (`work_membership`), so it expires them.
    let sql = format!(
        "SELECT seq,src,id FROM (
           SELECT 0 AS seq,0 AS src,id FROM work_items WHERE id=(SELECT parent_feature_id FROM work_items WHERE id=?1)
           UNION ALL SELECT 0,0,id FROM work_items WHERE parent_feature_id=?1
           UNION ALL SELECT seq,1,id FROM history_links WHERE root_id=?1
           UNION ALL SELECT seq,2,id FROM events WHERE root_id=?1 AND aggregate_kind IN ({LINEAGE_EVENT_AGGREGATES})
         ) WHERE seq>?2 OR (seq=?2 AND (src>?3 OR (src=?3 AND id>?4)))
         ORDER BY seq,src,id LIMIT ?5"
    );
    let mut stmt = db(conn.prepare_cached(&sql))?;
    let rows = db(stmt.query_map(
        params![root.0.as_str(), key.sequence, key.source as i64, key.id, n],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    ))?;
    let keys: Vec<(i64, i64, String)> = db(rows.collect())?;
    let mut out = Vec::with_capacity(keys.len());
    for (seq, src, id) in keys {
        let key = SequenceKey {
            sequence: seq,
            source: src as u8,
            id: id.clone(),
        };
        let row = match src {
            0 => LineageRow::Work {
                key,
                work: work_root(conn, &RootId(id_of(id)?))?
                    .ok_or_else(|| corrupt("lineage work item vanished"))?,
            },
            1 => LineageRow::Link {
                key,
                link: link(conn, &id_of(id)?)?.ok_or_else(|| corrupt("lineage link vanished"))?,
            },
            _ => {
                let raw = db(db(
                    conn.prepare_cached(&format!("SELECT {EVENT_COLUMNS} FROM events WHERE id=?1"))
                )?
                .query_row([id.as_str()], event_row))?;
                LineageRow::Event {
                    key,
                    event: event_of(raw)?,
                }
            }
        };
        out.push(row);
    }
    Ok(out)
}

fn root_events(
    conn: &Connection,
    root: &RootId,
    kinds: &[&str],
    after: i64,
    limit: u16,
) -> Result<Vec<EventRecord>, StorageError> {
    let n = bounded(limit)?;
    let mut stmt = db(conn.prepare_cached(&format!(
        "SELECT {EVENT_COLUMNS} FROM events WHERE root_id=?1 AND seq>?2
         AND kind IN (SELECT value FROM json_each(?3))
         AND json_extract(payload_json,'$.subject.kind') IS NOT NULL ORDER BY seq LIMIT ?4"
    )))?;
    let rows = db(stmt.query_map(
        params![root.0.as_str(), after, kinds_json(kinds), n],
        event_row,
    ))?;
    db(rows.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(event_of)
        .collect()
}

/// The column and value a scope selects (`IS`, so the workspace scope is the NULL project).
fn scope_column(scope: &HistoryScope) -> (&'static str, Option<&str>) {
    match scope {
        HistoryScope::Root(r) => ("root_id", Some(r.0.as_str())),
        HistoryScope::Project(p) => ("project_id", Some(p.0.as_str())),
        HistoryScope::Workspace => ("project_id", None),
    }
}

/// One source's page of usage facts after the keyset position `(?2 sequence, ?3 source, ?4 id)`;
/// `src` is the source's key and `id` its key ID expression. The `>=` term lets the per-scope
/// index seek to the position.
fn after_key(seq: &str, src: u8, id: &str) -> String {
    format!("{seq}>=?2 AND ({seq}>?2 OR ({src}>?3 OR ({src}=?3 AND {id}>?4)))")
}

/// The completed tool invocations of a scope after a keyset position, in committed order (one
/// statement over the indexed `tool_invocations`).
pub(crate) fn invocations_sql(column: &str) -> String {
    format!(
        "SELECT session_id,response_seq,call_seq,committed_seq,project_id,root_id,call_entry_id,
         capability_id,outcome,error_code,operation_id,actor_id,called_at,responded_at
         FROM tool_invocations WHERE {column} IS ?1 AND {}
         ORDER BY committed_seq,response_seq LIMIT ?5",
        after_key("committed_seq", 1, "printf('%012d',response_seq)")
    )
}

fn usage_facts(
    conn: &Connection,
    scope: Option<&HistoryScope>,
    after: Option<&SequenceKey>,
    limit: u16,
) -> Result<Vec<UsageFact>, StorageError> {
    let n = bounded(limit)?;
    let key = after.cloned().unwrap_or(SequenceKey {
        sequence: -1,
        source: 0,
        id: String::new(),
    });
    let (column, value) = scope.map_or(("NULL", None), scope_column);
    let keyed = params![value, key.sequence, key.source as i64, key.id, n];
    // Each source pages itself in key order; the merged page keeps the first `limit` keys.
    let mut out = Vec::new();
    let mut tools = db(conn.prepare_cached(&invocations_sql(column)))?;
    type ToolRaw = (
        String,
        i64,
        i64,
        i64,
        Option<String>,
        Option<String>,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        i64,
        i64,
    );
    let rows = db(tools.query_map(keyed, |r| {
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
        ))
    }))?;
    let raw: Vec<ToolRaw> = db(rows.collect())?;
    for (
        session,
        response,
        call,
        committed,
        project,
        root,
        entry,
        capability,
        outcome,
        code,
        op,
        actor,
        called,
        responded,
    ) in raw
    {
        out.push(UsageFact::Tool {
            key: SequenceKey {
                sequence: committed,
                source: 1,
                id: format!("{response:012}"),
            },
            invocation: StoredInvocation {
                session_id: id_of(session)?,
                response_sequence: response,
                call_sequence: call,
                committed_sequence: committed,
                project_id: opt_id(project)?.map(ProjectId),
                root_id: opt_id(root)?.map(RootId),
                call_entry_id: id_of(entry)?,
                capability_id: capability,
                outcome,
                error_code: code,
                operation_id: opt_id(op)?.map(OperationId),
                actor: ActorId(id_of(actor)?),
                called_at_ms: called,
                responded_at_ms: responded,
            },
        });
    }
    type UsageRaw = (
        String,
        i64,
        String,
        Option<String>,
        Option<String>,
        i64,
        String,
    );
    let mut usage = db(conn.prepare_cached(&format!(
        "SELECT batch_id,ordinal,session_id,project_id,root_id,committed_seq,item_json
         FROM session_usage WHERE {column} IS ?1 AND {}
         ORDER BY committed_seq,ordinal LIMIT ?5",
        after_key("committed_seq", 3, "printf('%06d',ordinal)")
    )))?;
    let rows = db(usage.query_map(keyed, |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
        ))
    }))?;
    let raw: Vec<UsageRaw> = db(rows.collect())?;
    for (b, o, s, p, r, c, item) in raw {
        out.push(UsageFact::Inference {
            key: SequenceKey {
                sequence: c,
                source: 3,
                id: format!("{o:06}"),
            },
            usage: StoredUsage {
                session_id: id_of(s)?,
                batch_id: id_of(b)?,
                ordinal: o,
                project_id: opt_id(p)?.map(ProjectId),
                root_id: opt_id(r)?.map(RootId),
                committed_sequence: c,
                item: json_of(&item)?,
            },
        });
    }
    out.sort_by(|a, b| a.key().cmp(b.key()));
    out.truncate(n as usize);
    Ok(out)
}

/// The `CommandDecided` events of a scope after a sequence, in committed order. The literal
/// kind selects the partial per-project index (`event_project_decisions`); a root's decisions
/// use the per-root index.
pub(crate) fn decisions_sql(column: &str) -> String {
    format!(
        "SELECT {EVENT_COLUMNS} FROM events WHERE {column} IS ?1 AND kind='CommandDecided'
         AND seq>?2 ORDER BY seq LIMIT ?3"
    )
}

fn decisions(
    conn: &Connection,
    scope: &HistoryScope,
    after: i64,
    limit: u16,
) -> Result<Vec<EventRecord>, StorageError> {
    let n = bounded(limit)?;
    let (column, value) = scope_column(scope);
    let mut stmt = db(conn.prepare_cached(&decisions_sql(column)))?;
    let rows = db(stmt.query_map(params![value, after, n], event_row))?;
    db(rows.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(event_of)
        .collect()
}

pub(crate) fn decisions_before_sql(column: &str, inclusive: bool) -> String {
    let comparison = if inclusive { "<=" } else { "<" };
    format!(
        "SELECT {EVENT_COLUMNS} FROM events WHERE {column} IS ?1 AND kind='CommandDecided' AND seq{comparison}?2 ORDER BY seq DESC LIMIT ?3"
    )
}

fn decisions_before(
    conn: &Connection,
    scope: &HistoryScope,
    before: Option<i64>,
    limit: u16,
) -> Result<Vec<EventRecord>, StorageError> {
    let n = bounded(limit)?;
    let (column, value) = scope_column(scope);
    let sql = decisions_before_sql(column, before.is_none());
    let mut statement = db(conn.prepare_cached(&sql))?;
    let rows = db(statement.query_map(params![value, before.unwrap_or(i64::MAX), n], event_row))?;
    db(rows.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(event_of)
        .collect()
}

macro_rules! session_reads {
    ($t:ty) => {
        impl SessionRead for $t {
            fn work_root(&self, root: &RootId) -> Result<Option<WorkRoot>, StorageError> {
                work_root(self.conn, root)
            }
            fn session(&self, id: &Id) -> Result<Option<StoredSession>, StorageError> {
                session(self.conn, id)
            }
            fn session_repository_scope(&self, id: &Id) -> Result<Vec<Id>, StorageError> {
                let raw: Option<String> = db(self.conn.query_row(
                    "SELECT repository_ids_json FROM sessions WHERE id=?1",
                    [id.as_str()], |row| row.get(0),
                ).optional())?;
                let raw = raw.ok_or(StorageError::ScopeDenied)?;
                let ids: Vec<Id> = serde_json::from_str::<Vec<String>>(&raw)
                    .map_err(|_| corrupt("session repository scope"))?
                    .into_iter().map(id_of).collect::<Result<_, _>>()?;
                if ids.windows(2).any(|pair| pair[0].as_str() >= pair[1].as_str()) {
                    return Err(corrupt("unordered session repository scope"));
                }
                Ok(ids)
            }
            fn session_activity_at_ms(&self, id: &Id) -> Result<Option<i64>, StorageError> {
                db(self.conn.query_row(
                    "SELECT last_seen_at FROM sessions WHERE id=?1",
                    [id.as_str()], |row| row.get(0),
                ).optional())?.ok_or(StorageError::ScopeDenied)
            }
            fn sessions(
                &self,
                scope: &SessionScope,
                closed: Option<bool>,
                after: Option<&SessionPosition>,
                limit: u16,
            ) -> Result<Vec<StoredSession>, StorageError> {
                sessions(self.conn, scope, closed, after, limit)
            }
            fn entries(
                &self,
                session: &Id,
                after: i64,
                limit: u16,
            ) -> Result<Vec<StoredEntry>, StorageError> {
                let n = bounded(limit)?;
                let mut stmt = db(self.conn.prepare_cached(&format!(
                    "SELECT {ENTRY_COLUMNS} FROM transcript_chunks WHERE session_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3"
                )))?;
                let rows = db(stmt.query_map(params![session.as_str(), after, n], entry_row))?;
                db(rows.collect::<rusqlite::Result<Vec<_>>>())?
                    .into_iter()
                    .map(entry_of)
                    .collect()
            }
            fn entry_exists(&self, session: &Id, entry_id: &Id) -> Result<bool, StorageError> {
                exists(
                    self.conn,
                    "SELECT 1 FROM transcript_chunks WHERE session_id=?1 AND entry_id=?2",
                    params![session.as_str(), entry_id.as_str()],
                )
            }
            fn last_entry_at_ms(&self, session: &Id) -> Result<Option<i64>, StorageError> {
                // The newest entry has the highest sequence (primary key order, one row).
                db(self
                    .conn
                    .query_row(
                        "SELECT created_at FROM transcript_chunks WHERE session_id=?1 ORDER BY seq DESC LIMIT 1",
                        params![session.as_str()],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional())
            }
            fn batch(
                &self,
                session: &Id,
                batch_id: &Id,
            ) -> Result<Option<StoredBatch>, StorageError> {
                batch(self.conn, session, batch_id)
            }
            fn work_membership(&self, root: &RootId) -> Result<String, StorageError> {
                let (parent, children): (Option<String>, Option<String>) = db(db(self
                    .conn
                    .prepare_cached(
                        "SELECT (SELECT parent_feature_id FROM work_items WHERE id=?1),
                         (SELECT group_concat(id,',') FROM (SELECT id FROM work_items WHERE parent_feature_id=?1 ORDER BY id))",
                    ))?
                .query_row([root.0.as_str()], |r| Ok((r.get(0)?, r.get(1)?))))?;
                Ok(devworks_core::fingerprint::sha256_hex(
                    format!(
                        "{}|{}",
                        parent.unwrap_or_default(),
                        children.unwrap_or_default()
                    )
                    .as_bytes(),
                ))
            }
            fn project_exists(&self, project: &ProjectId) -> Result<bool, StorageError> {
                exists(
                    self.conn,
                    "SELECT 1 FROM projects WHERE id=?1",
                    [project.0.as_str()],
                )
            }
            fn project_scope_revision(
                &self,
                project: &ProjectId,
            ) -> Result<Option<i64>, StorageError> {
                db(db(self
                    .conn
                    .prepare_cached("SELECT scope_revision FROM projects WHERE id=?1"))?
                .query_row([project.0.as_str()], |r| r.get(0))
                .optional())
            }
            fn project_has_repository(
                &self,
                project: &ProjectId,
                repository: &Id,
            ) -> Result<bool, StorageError> {
                exists(
                    self.conn,
                    "SELECT 1 FROM project_repositories WHERE project_id=?1 AND repository_id=?2",
                    params![project.0.as_str(), repository.as_str()],
                )
            }
            fn tool_call(
                &self,
                session: &Id,
                attempt_id: &Id,
                native_call_id: &str,
            ) -> Result<Option<StoredToolCall>, StorageError> {
                let raw = db(db(self.conn.prepare_cached(&format!(
                    "SELECT {CALL_COLUMNS} FROM session_tool_calls WHERE session_id=?1 AND attempt_id=?2 AND native_call_id=?3"
                )))?
                .query_row(
                    params![session.as_str(), attempt_id.as_str(), native_call_id],
                    call_row,
                )
                .optional())?;
                raw.map(call_of).transpose()
            }
            fn ordinal_taken(
                &self,
                session: &Id,
                attempt_id: &Id,
                ordinal: i64,
            ) -> Result<bool, StorageError> {
                exists(
                    self.conn,
                    "SELECT 1 FROM session_tool_calls WHERE session_id=?1 AND attempt_id=?2 AND ordinal=?3",
                    params![session.as_str(), attempt_id.as_str(), ordinal],
                )
            }
            fn pending_calls(
                &self,
                session: &Id,
                limit: u16,
            ) -> Result<Vec<StoredToolCall>, StorageError> {
                calls(self.conn, "response_seq IS NULL ORDER BY call_seq", session, limit)
            }
            fn linked_operations(
                &self,
                session: &Id,
                limit: u16,
            ) -> Result<Vec<StoredToolCall>, StorageError> {
                calls(
                    self.conn,
                    "operation_id IS NOT NULL ORDER BY response_seq",
                    session,
                    limit,
                )
            }
            fn epoch(&self, session: &Id, e: i64) -> Result<Option<StoredEpoch>, StorageError> {
                epoch(self.conn, session, e)
            }
            fn provider_context_head(
                &self,
                session: &Id,
            ) -> Result<Option<StoredProviderContextHead>, StorageError> {
                provider_context_head(self.conn, session)
            }
            fn context_sources(
                &self,
                session: &Id,
                epoch: i64,
                after: i64,
                limit: u16,
            ) -> Result<Vec<StoredContextSource>, StorageError> {
                context_sources(self.conn, session, epoch, after, limit)
            }
            fn latest_checkpoint(
                &self,
                session: &Id,
            ) -> Result<Option<StoredCheckpoint>, StorageError> {
                latest_checkpoint(self.conn, session)
            }
            fn usage(
                &self,
                root: &RootId,
                after: Option<&SequenceKey>,
                limit: u16,
            ) -> Result<Vec<(SequenceKey, StoredUsage)>, StorageError> {
                usage(self.conn, root, after, limit)
            }
            fn all_usage_facts(&self,after:Option<&SequenceKey>,limit:u16)->Result<Vec<UsageFact>,StorageError>{usage_facts(self.conn,None,after,limit)}
            fn usage_facts(
                &self,
                scope: &HistoryScope,
                after: Option<&SequenceKey>,
                limit: u16,
            ) -> Result<Vec<UsageFact>, StorageError> {
                usage_facts(self.conn, Some(scope), after, limit)
            }
            fn link(&self, id: &Id) -> Result<Option<StoredLink>, StorageError> {
                link(self.conn, id)
            }
            fn links(
                &self,
                root: &RootId,
                kinds: &[&str],
                after: Option<&SequenceKey>,
                limit: u16,
            ) -> Result<Vec<StoredLink>, StorageError> {
                links(self.conn, root, kinds, after, limit)
            }
            fn links_to(
                &self,
                target_kind: &str,
                target_id: &str,
                limit: u16,
            ) -> Result<Vec<StoredLink>, StorageError> {
                links_to(self.conn, target_kind, target_id, limit)
            }
            fn lineage(
                &self,
                root: &RootId,
                after: Option<&SequenceKey>,
                limit: u16,
            ) -> Result<Vec<LineageRow>, StorageError> {
                lineage(self.conn, root, after, limit)
            }
            fn root_events(
                &self,
                root: &RootId,
                kinds: &[&str],
                after: i64,
                limit: u16,
            ) -> Result<Vec<EventRecord>, StorageError> {
                root_events(self.conn, root, kinds, after, limit)
            }
            fn decisions(
                &self,
                scope: &HistoryScope,
                after: i64,
                limit: u16,
            ) -> Result<Vec<EventRecord>, StorageError> {
                decisions(self.conn, scope, after, limit)
            }
            fn decisions_before(&self, scope: &HistoryScope, before: Option<i64>, limit: u16)
                -> Result<Vec<EventRecord>, StorageError> {
                decisions_before(self.conn,scope,before,limit)
            }

        }
    };
}
session_reads!(Snapshot<'_>);
session_reads!(Unit<'_>);

// ---------------------------------------------------------------------------------------------
// Writes

fn json_text(v: &Value) -> String {
    v.to_string()
}

impl SessionRepository for Unit<'_> {
    fn insert_session(&mut self, s: &StoredSession) -> Result<(), StorageError> {
        text("session title", &s.title, 4096)?;
        if s.revision <= 0 || s.next_sequence <= 0 || s.context_epoch < 0 {
            return Err(invalid("session counters out of range"));
        }
        if let Some(p) = &s.project_id {
            require(
                self.conn,
                "SELECT 1 FROM projects WHERE id=?1 AND lifecycle<>'Deleted'",
                [p.0.as_str()],
                "project",
                p.0.as_str(),
            )?;
        }
        if let Some(r) = &s.root_id {
            require(
                self.conn,
                "SELECT 1 FROM work_items WHERE id=?1",
                [r.0.as_str()],
                "root",
                r.0.as_str(),
            )?;
        }
        if exists(
            self.conn,
            "SELECT 1 FROM sessions WHERE id=?1",
            [s.id.as_str()],
        )? {
            return Err(StorageError::Duplicate {
                kind: "session",
                id: s.id.as_str().to_owned(),
            });
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO conversations(id,root_id,created_at) VALUES(?1,?2,?3)",
        ))?
        .execute(params![
            s.conversation_id.as_str(),
            s.root_id.as_ref().map(|r| r.0.as_str()),
            s.created_at_ms
        ]))?;
        // A migration-compatibility reader may open an older store before migration 44 added
        // the immutable repository scope. Its existing sessions still use the historical shape.
        let schema_version: i64 = db(self.conn.query_row(
            "SELECT schema_version FROM store_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        ))?;
        if schema_version < 44 {
            db(db(self.conn.prepare_cached(
                "INSERT INTO sessions(id,conversation_id,agent_id,root_id,context_epoch,revision,pin_json,project_id,title,created_by,created_at,closed_at,closed_by,next_seq,checkpoint_seq) VALUES(?1,?2,?3,?4,?5,?6,NULL,?7,?8,?9,?10,?11,?12,?13,?14)",
            ))?.execute(params![
                s.id.as_str(), s.conversation_id.as_str(), s.agent_id.0.as_str(),
                s.root_id.as_ref().map(|r| r.0.as_str()), s.context_epoch, s.revision,
                s.project_id.as_ref().map(|p| p.0.as_str()), s.title,
                s.created_by.0.as_str(), s.created_at_ms, s.closed_at_ms,
                s.closed_by.as_ref().map(|a| a.0.as_str()), s.next_sequence, s.checkpoint_sequence,
            ]))?;
            return Ok(());
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO sessions(id,conversation_id,agent_id,root_id,context_epoch,revision,pin_json,project_id,title,created_by,created_at,closed_at,closed_by,next_seq,checkpoint_seq,repository_ids_json) VALUES(?1,?2,?3,?4,?5,?6,NULL,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        ))?
        .execute(params![
            s.id.as_str(),
            s.conversation_id.as_str(),
            s.agent_id.0.as_str(),
            s.root_id.as_ref().map(|r| r.0.as_str()),
            s.context_epoch,
            s.revision,
            s.project_id.as_ref().map(|p| p.0.as_str()),
            s.title,
            s.created_by.0.as_str(),
            s.created_at_ms,
            s.closed_at_ms,
            s.closed_by.as_ref().map(|a| a.0.as_str()),
            s.next_sequence,
            s.checkpoint_sequence,
            {
                let repositories: Vec<String> = match &s.root_id {
                    Some(root) => {
                        let mut statement = db(self.conn.prepare(
                            "SELECT repository_id FROM work_repositories WHERE work_id=?1 ORDER BY repository_id"
                        ))?;
                        let rows = db(statement.query_map([root.0.as_str()], |row| row.get::<_, String>(0)))?;
                        rows.map(db).collect::<Result<Vec<_>, _>>()?
                    }
                    None => Vec::new(),
                };
                serde_json::to_string(&repositories).map_err(|_| invalid("session repository scope"))?
            }
        ]))?;
        Ok(())
    }

    fn update_session(&mut self, s: &StoredSession) -> Result<(), StorageError> {
        text("session title", &s.title, 4096)?;
        if s.revision <= 0 || s.next_sequence <= 0 || s.context_epoch < 0 {
            return Err(invalid("session counters out of range"));
        }
        let n = db(db(self.conn.prepare_cached(
            "UPDATE sessions SET title=?2,context_epoch=?3,revision=?4,closed_at=?5,closed_by=?6,next_seq=?7,checkpoint_seq=?8 WHERE id=?1",
        ))?
        .execute(params![
            s.id.as_str(),
            s.title,
            s.context_epoch,
            s.revision,
            s.closed_at_ms,
            s.closed_by.as_ref().map(|a| a.0.as_str()),
            s.next_sequence,
            s.checkpoint_sequence
        ]))?;
        if n != 1 {
            return Err(StorageError::NotFound {
                kind: "session",
                id: s.id.as_str().to_owned(),
            });
        }
        Ok(())
    }

    fn touch_session_activity(&mut self, id: &Id, at_ms: i64) -> Result<(), StorageError> {
        if at_ms < 0 {
            return Err(invalid("negative session activity time"));
        }
        let updated = db(self.conn.execute(
            "UPDATE sessions SET last_seen_at=MAX(COALESCE(last_seen_at, created_at), ?2) WHERE id=?1 AND closed_at IS NULL",
            params![id.as_str(), at_ms],
        ))?;
        if updated != 1 {
            return Err(StorageError::ScopeDenied);
        }
        Ok(())
    }

    fn insert_entry(&mut self, e: &StoredEntry) -> Result<(), StorageError> {
        text("entry kind", &e.kind, 64)?;
        if e.sequence <= 0 || !e.body.is_object() {
            return Err(invalid("transcript entry sequence or body"));
        }
        if self.entry_exists(&e.session_id, &e.entry_id)? {
            return Err(StorageError::Duplicate {
                kind: "transcript_entry",
                id: e.entry_id.as_str().to_owned(),
            });
        }
        if exists(
            self.conn,
            "SELECT 1 FROM transcript_chunks WHERE session_id=?1 AND seq=?2",
            params![e.session_id.as_str(), e.sequence],
        )? {
            return Err(StorageError::Duplicate {
                kind: "transcript_sequence",
                id: e.sequence.to_string(),
            });
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO transcript_chunks(session_id,seq,provider_attempt_id,kind,payload_json,entry_id,batch_id,context_epoch,actor_id,created_at) VALUES(?1,?2,NULL,?3,?4,?5,?6,?7,?8,?9)",
        ))?
        .execute(params![
            e.session_id.as_str(),
            e.sequence,
            e.kind,
            json_text(&e.body),
            e.entry_id.as_str(),
            e.batch_id.as_str(),
            e.context_epoch,
            e.actor.0.as_str(),
            e.created_at_ms
        ]))?;
        Ok(())
    }

    fn insert_batch(&mut self, b: &StoredBatch) -> Result<(), StorageError> {
        if batch(self.conn, &b.session_id, &b.batch_id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "transcript_batch",
                id: b.batch_id.as_str().to_owned(),
            });
        }
        db(db(self.conn.prepare_cached(&format!(
            "INSERT INTO transcript_batches({BATCH_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)"
        )))?
        .execute(params![
            b.batch_id.as_str(),
            b.session_id.as_str(),
            b.payload_digest,
            b.actor.0.as_str(),
            b.context_epoch,
            b.first_sequence,
            b.last_sequence,
            b.event_id.as_str(),
            b.committed_sequence,
            b.accepted_at_ms
        ]))?;
        Ok(())
    }

    fn insert_tool_call(&mut self, c: &StoredToolCall) -> Result<(), StorageError> {
        text("native call ID", &c.native_call_id, 1024)?;
        text("capability ID", &c.capability_id, 256)?;
        if self
            .tool_call(&c.session_id, &c.attempt_id, &c.native_call_id)?
            .is_some()
            || self.ordinal_taken(&c.session_id, &c.attempt_id, c.ordinal)?
        {
            return Err(StorageError::Duplicate {
                kind: "tool_call",
                id: format!("{}:{}", c.attempt_id.as_str(), c.native_call_id),
            });
        }
        db(db(self.conn.prepare_cached(&format!(
            "INSERT INTO session_tool_calls({CALL_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,NULL,NULL,NULL)"
        )))?
        .execute(params![
            c.session_id.as_str(),
            c.attempt_id.as_str(),
            c.native_call_id,
            c.ordinal,
            c.capability_id,
            c.call_sequence
        ]))?;
        Ok(())
    }

    fn record_response(&mut self, c: &StoredToolCall) -> Result<(), StorageError> {
        let (Some(seq), Some(outcome)) = (c.response_sequence, c.outcome.as_deref()) else {
            return Err(invalid("a response needs its sequence and outcome"));
        };
        text("response outcome", outcome, 64)?;
        if let Some(op) = &c.operation_id {
            require(
                self.conn,
                "SELECT 1 FROM operations WHERE id=?1",
                [op.0.as_str()],
                "operation",
                op.0.as_str(),
            )?;
        }
        // Written once: the slot is only filled while it is empty.
        let n = db(db(self.conn.prepare_cached(
            "UPDATE session_tool_calls SET response_seq=?4,outcome=?5,operation_id=?6 WHERE session_id=?1 AND attempt_id=?2 AND native_call_id=?3 AND response_seq IS NULL",
        ))?
        .execute(params![
            c.session_id.as_str(),
            c.attempt_id.as_str(),
            c.native_call_id,
            seq,
            outcome,
            c.operation_id.as_ref().map(|o| o.0.as_str())
        ]))?;
        if n != 1 {
            return Err(StorageError::Duplicate {
                kind: "tool_response",
                id: format!("{}:{}", c.attempt_id.as_str(), c.native_call_id),
            });
        }
        Ok(())
    }

    fn insert_epoch(&mut self, e: &StoredEpoch) -> Result<(), StorageError> {
        db(db(self.conn.prepare_cached(
            "INSERT INTO session_context_epochs(session_id,epoch,previous_epoch,reason,context_json,identity,seq,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        ))?
        .execute(params![
            e.session_id.as_str(),
            e.epoch,
            e.previous_epoch,
            e.reason,
            json_text(&e.context),
            e.identity,
            e.sequence,
            e.created_at_ms
        ]))?;
        Ok(())
    }

    fn put_provider_context_head(
        &mut self,
        head: &StoredProviderContextHead,
        expected_revision: Option<i64>,
    ) -> Result<(), StorageError> {
        if head.context_epoch < 0
            || head.observed_sequence < 0
            || expected_revision.unwrap_or(0).checked_add(1) != Some(head.revision)
            || !matches!(
                head.kind.as_str(),
                "ProviderInitialContext"
                    | "ProviderTurnContinuation"
                    | "ProviderOperationUpdateContext"
            )
            || match head.kind.as_str() {
                "ProviderInitialContext" => {
                    head.context_epoch != 0
                        || head.observed_sequence != 0
                        || head.attempt_id.is_some()
                }
                "ProviderTurnContinuation" => head.attempt_id.is_none(),
                "ProviderOperationUpdateContext" => head.attempt_id.is_some(),
                _ => true,
            }
        {
            return Err(invalid("provider context head revision or position"));
        }
        let binding = json_text(&head.binding);
        let artifact = json_text(&head.artifact);
        let changed = match expected_revision {
            Some(expected) => db(db(self.conn.prepare_cached(
                "UPDATE session_provider_context_heads SET context_epoch=?2,observed_sequence=?3,kind=?4,attempt_id=?5,binding_json=?6,artifact_json=?7,revision=?8 WHERE session_id=?1 AND revision=?9",
            ))?.execute(params![head.session_id.as_str(),head.context_epoch,head.observed_sequence,head.kind,head.attempt_id.as_ref().map(Id::as_str),binding,artifact,head.revision,expected]))?,
            None => db(db(self.conn.prepare_cached(
                "INSERT INTO session_provider_context_heads(session_id,context_epoch,observed_sequence,kind,attempt_id,binding_json,artifact_json,revision) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            ))?.execute(params![head.session_id.as_str(),head.context_epoch,head.observed_sequence,head.kind,head.attempt_id.as_ref().map(Id::as_str),binding,artifact,head.revision]))?,
        };
        if changed != 1 {
            return Err(StorageError::ScopeDenied);
        }
        Ok(())
    }

    fn insert_context_source(&mut self, s: &StoredContextSource) -> Result<(), StorageError> {
        text("context source kind", &s.kind, 256)?;
        db(db(self.conn.prepare_cached(
            "INSERT INTO session_context_sources(session_id,epoch,ordinal,source_id,kind,source_revision,artifact_json,token_estimate,disposition,seq) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        ))?
        .execute(params![
            s.session_id.as_str(),
            s.epoch,
            s.ordinal,
            s.source_id.as_str(),
            s.kind,
            s.source_revision,
            s.artifact.as_ref().map(json_text),
            s.token_estimate,
            s.disposition,
            s.sequence
        ]))?;
        Ok(())
    }

    fn insert_checkpoint(&mut self, c: &StoredCheckpoint) -> Result<(), StorageError> {
        db(db(self.conn.prepare_cached(
            "INSERT INTO session_checkpoints(session_id,seq,batch_id,artifact_json,created_at) VALUES(?1,?2,?3,?4,?5)",
        ))?
        .execute(params![
            c.session_id.as_str(),
            c.sequence,
            c.batch_id.as_str(),
            json_text(&c.artifact),
            c.created_at_ms
        ]))?;
        Ok(())
    }

    fn insert_usage(&mut self, u: &StoredUsage) -> Result<(), StorageError> {
        if !u.item.is_object() {
            return Err(invalid("usage item must be a JSON object"));
        }
        db(db(self.conn.prepare_cached(
            "INSERT INTO session_usage(batch_id,ordinal,session_id,project_id,root_id,committed_seq,item_json) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        ))?
        .execute(params![
            u.batch_id.as_str(),
            u.ordinal,
            u.session_id.as_str(),
            u.project_id.as_ref().map(|p| p.0.as_str()),
            u.root_id.as_ref().map(|r| r.0.as_str()),
            u.committed_sequence,
            json_text(&u.item)
        ]))?;
        Ok(())
    }

    fn insert_invocation(&mut self, v: &StoredInvocation) -> Result<(), StorageError> {
        text("tool capability", &v.capability_id, 1024)?;
        db(db(self.conn.prepare_cached(
            "INSERT INTO tool_invocations(session_id,response_seq,call_seq,committed_seq,project_id,
             root_id,call_entry_id,capability_id,outcome,error_code,operation_id,actor_id,called_at,
             responded_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        ))?
        .execute(params![
            v.session_id.as_str(),
            v.response_sequence,
            v.call_sequence,
            v.committed_sequence,
            v.project_id.as_ref().map(|p| p.0.as_str()),
            v.root_id.as_ref().map(|r| r.0.as_str()),
            v.call_entry_id.as_str(),
            v.capability_id,
            v.outcome,
            v.error_code,
            v.operation_id.as_ref().map(|o| o.0.as_str()),
            v.actor.0.as_str(),
            v.called_at_ms,
            v.responded_at_ms
        ]))?;
        Ok(())
    }

    fn insert_link(&mut self, l: &StoredLink) -> Result<(), StorageError> {
        text("link kind", &l.kind, 64)?;
        text("link target kind", &l.target_kind, 64)?;
        text("link target", &l.target_id, 1024)?;
        text("link summary", &l.summary, 4096)?;
        if !l.body.is_object() {
            return Err(invalid("link body must be a JSON object"));
        }
        for prior in [&l.parent_id, &l.supersedes_id].into_iter().flatten() {
            require(
                self.conn,
                "SELECT 1 FROM history_links WHERE id=?1",
                [prior.as_str()],
                "history_link",
                prior.as_str(),
            )?;
        }
        if link(self.conn, &l.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "history_link",
                id: l.id.as_str().to_owned(),
            });
        }
        db(db(self.conn.prepare_cached(&format!(
            "INSERT INTO history_links({LINK_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)"
        )))?
        .execute(params![
            l.id.as_str(),
            l.project_id.0.as_str(),
            l.root_id.0.as_str(),
            l.kind,
            l.target_kind,
            l.target_id,
            l.parent_id.as_ref().map(Id::as_str),
            l.supersedes_id.as_ref().map(Id::as_str),
            l.actor.0.as_str(),
            l.event_id.as_str(),
            l.sequence,
            l.repository_id.as_ref().map(Id::as_str),
            l.revision,
            l.summary,
            json_text(&l.body),
            l.created_at_ms
        ]))?;
        Ok(())
    }
}
