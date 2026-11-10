//! Narrow membership handoff: a superseded worker changes scope only at a quiescent boundary.
use super::conn::db;
use super::repos::Unit;
use devworks_core::ports::storage::*;
use devworks_core::protocol::{AssignmentId, Id, Revision, RevisionDimension};
use rusqlite::{Connection, params};

fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn conflict(
    kind: &'static str,
    id: &Id,
    expected: i64,
    current: i64,
) -> Result<StorageError, StorageError> {
    Ok(StorageError::conflict(
        RevisionDimension::Work,
        kind,
        id.clone(),
        Revision::new(expected).map_err(|_| invalid("expected revision is positive"))?,
        Revision::new(current).map_err(|_| invalid("stored revision is positive"))?,
    ))
}
fn preserved_checkpoint(old: &StoredAssignment) -> Result<Option<serde_json::Value>, StorageError> {
    let value = old
        .checkpoint
        .as_ref()
        .ok_or_else(|| invalid("membership handoff requires its service checkpoint marker"))?;
    if value.get("kind").and_then(|v| v.as_str()) != Some("DevWorksMembershipHandoff")
        || value.get("version").and_then(|v| v.as_i64()) != Some(1)
    {
        return Err(invalid(
            "membership handoff requires its service checkpoint marker",
        ));
    }
    let marker = value
        .get("membershipHandoff")
        .ok_or_else(|| invalid("membership handoff marker is absent"))?;
    match marker.get("hadCheckpoint").and_then(|v| v.as_bool()) {
        Some(false) => Ok(None),
        Some(true) => Ok(Some(
            marker
                .get("previousCheckpoint")
                .ok_or_else(|| invalid("membership handoff prior checkpoint is absent"))?
                .clone(),
        )),
        None => Err(invalid("membership handoff checkpoint presence is absent")),
    }
}

pub(super) fn retire(
    unit: &mut Unit<'_>,
    assignment: &AssignmentId,
    expected: i64,
) -> Result<(), StorageError> {
    let old = unit
        .assignment(assignment)?
        .ok_or_else(|| StorageError::NotFound {
            kind: "assignment",
            id: assignment.0.as_str().into(),
        })?;
    if old.revision != expected {
        return Err(conflict(
            "assignment",
            &assignment.0,
            expected,
            old.revision,
        )?);
    }
    if old.state != AssignmentState::Superseded || old.slot_held {
        return Err(invalid(
            "membership retirement requires a superseded assignment",
        ));
    }
    let checkpoint = preserved_checkpoint(&old)?;
    let next = expected
        .checked_add(1)
        .ok_or_else(|| invalid("assignment revision overflow"))?;
    db(unit.conn.execute(
        "UPDATE assignments SET checkpoint_json=?2,revision=?3 WHERE id=?1 AND revision=?4",
        params![
            assignment.0.as_str(),
            checkpoint.as_ref().map(serde_json::Value::to_string),
            next,
            expected
        ],
    ))?;
    Ok(())
}

pub(super) fn reparent(
    unit: &mut Unit<'_>,
    a: &StoredAssignment,
    expected: i64,
    expected_session: i64,
) -> Result<(), StorageError> {
    let old = unit
        .assignment(&a.id)?
        .ok_or_else(|| StorageError::NotFound {
            kind: "assignment",
            id: a.id.0.as_str().into(),
        })?;
    if old.revision != expected {
        return Err(conflict("assignment", &a.id.0, expected, old.revision)?);
    }
    if old.state != AssignmentState::Superseded
        || old.slot_held
        || a.state != AssignmentState::Waiting
        || a.slot_held
        || a.work_id != old.work_id
        || a.session_id != old.session_id
        || a.created_at_ms != old.created_at_ms
        || a.revision
            != expected
                .checked_add(1)
                .ok_or_else(|| invalid("assignment revision overflow"))?
        || a.generation
            != old
                .generation
                .checked_add(1)
                .ok_or_else(|| invalid("assignment generation overflow"))?
        || a.instruction_revision < 0
        || a.scope_key.as_ref().is_some_and(|v| v.len() > 256)
        || a.checkpoint != preserved_checkpoint(&old)?
    {
        return Err(invalid(
            "handoff keeps worker/session/checkpoint and advances a superseded worker exactly once into Waiting",
        ));
    }
    let work = WorkRead::work(unit, &a.work_id.0)?.ok_or_else(|| StorageError::NotFound {
        kind: "work",
        id: a.work_id.0.as_str().into(),
    })?;
    if work.is_feature() || work.parent_feature_id.as_ref().unwrap_or(&work.id) != &a.root_id.0 {
        return Err(invalid(
            "handoff destination is the task's current membership",
        ));
    }
    let old_root = WorkRead::work(unit, &old.root_id.0)?
        .ok_or_else(|| invalid("handoff source root is absent"))?;
    let target = WorkRead::work(unit, &a.root_id.0)?
        .ok_or_else(|| invalid("handoff destination root is absent"))?;
    if old_root.project_id != work.project_id || target.project_id != work.project_id {
        return Err(StorageError::ScopeDenied);
    }
    if work.parent_feature_id.is_some() {
        let parent = a
            .parent_id
            .as_ref()
            .ok_or_else(|| invalid("feature handoff waits for its destination coordinator"))?;
        let parent = unit
            .assignment(parent)?
            .ok_or_else(|| invalid("destination coordinator is absent"))?;
        if parent.id == a.id
            || !parent.state.is_live()
            || parent.root_id != a.root_id
            || parent.work_id != a.root_id
            || parent.parent_id.is_some()
        {
            return Err(invalid(
                "handoff parent is its live destination coordinator",
            ));
        }
    } else if a.parent_id.is_some() {
        return Err(invalid("standalone handoff has no coordinator parent"));
    }
    if let Some(execution) = &a.execution_id {
        let activation = unit
            .activation(execution)?
            .ok_or_else(|| invalid("handoff target execution is absent"))?;
        if activation.root_id != a.root_id
            || !activation.phase.is_live()
            || activation.phase == ActivationPhase::Stopping
            || Some(&activation.boot_id) != unit.store().boot_id.as_ref()
        {
            return Err(invalid(
                "handoff execution is a current live destination activation",
            ));
        }
    }
    let session = unit
        .session(&a.session_id)?
        .ok_or_else(|| StorageError::NotFound {
            kind: "session",
            id: a.session_id.as_str().into(),
        })?;
    if session.revision != expected_session {
        return Err(conflict(
            "session",
            &session.id,
            expected_session,
            session.revision,
        )?);
    }
    if session.closed()
        || session.root_id.as_ref() != Some(&old.root_id)
        || session.project_id.as_ref() != Some(&work.project_id)
    {
        return Err(invalid(
            "handoff session keeps the source worker's project and root",
        ));
    }
    let epoch = session
        .context_epoch
        .checked_add(1)
        .ok_or_else(|| invalid("session context epoch overflow"))?;
    let session_revision = session
        .revision
        .checked_add(1)
        .ok_or_else(|| invalid("session revision overflow"))?;
    let conversation_ok:bool=db(unit.conn.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1 AND root_id=?2) AND NOT EXISTS(SELECT 1 FROM sessions WHERE conversation_id=?1 AND id<>?3)",params![session.conversation_id.as_str(),old.root_id.0.as_str(),session.id.as_str()],|row|row.get(0)))?;
    if !conversation_ok {
        return Err(invalid(
            "handoff conversation belongs only to its source session",
        ));
    }
    // The read fact and mutation use the same canonical operation-state rule.
    if !quiescent(unit.conn, &a.id)? {
        return Err(StorageError::Busy);
    }
    // A caller may catch a repository error and continue its write. Keep this multi-row
    // primitive atomic even then (for example if moving a blocker hits a uniqueness guard).
    let savepoint = unit.savepoint()?;
    let result = (|| {
        db(unit.conn.execute("UPDATE assignments SET root_id=?2,parent_id=?3,execution_id=?4,state='Waiting',generation=?5,slot_held=0,revision=?6,checkpoint_json=?7,scope_key=?8,scope_digest=?9,plan_revision=?10,instruction_revision=?11 WHERE id=?1 AND revision=?12",params![a.id.0.as_str(),a.root_id.0.as_str(),a.parent_id.as_ref().map(|p|p.0.as_str()),a.execution_id.as_ref().map(Id::as_str),a.generation,a.revision,a.checkpoint.as_ref().map(serde_json::Value::to_string),a.scope_key,a.scope_digest,a.plan_revision,a.instruction_revision,expected]))?;
        db(unit.conn.execute("UPDATE sessions SET root_id=?2,context_epoch=?3,revision=?4 WHERE id=?1 AND revision=?5",params![session.id.as_str(),a.root_id.0.as_str(),epoch,session_revision,expected_session]))?;
        db(unit.conn.execute(
            "UPDATE conversations SET root_id=?2 WHERE id=?1",
            params![session.conversation_id.as_str(), a.root_id.0.as_str()],
        ))?;
        db(unit.conn.execute("UPDATE blockers SET root_id=?2,revision=revision+1 WHERE state='Active' AND (root_id=?3 OR root_id IS NULL) AND (assignment_id=?1 OR operation_id IN (SELECT id FROM operations WHERE assignment_id=?1))",params![a.id.0.as_str(),a.root_id.0.as_str(),old.root_id.0.as_str()]))?;
        Ok(())
    })();
    match result {
        Ok(()) => unit.release(savepoint),
        Err(error) => {
            unit.rollback_to(savepoint)?;
            Err(error)
        }
    }
}

/// Shared by read snapshots and the mutation: callers may retire an abandoned handoff only
/// after the same boundary the reparent primitive enforces.
pub(super) fn quiescent(
    conn: &Connection,
    assignment: &devworks_core::protocol::AssignmentId,
) -> Result<bool, StorageError> {
    let exists: bool = db(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM assignments WHERE id=?1)",
        [assignment.0.as_str()],
        |row| row.get(0),
    ))?;
    if !exists {
        return Err(StorageError::NotFound {
            kind: "assignment",
            id: assignment.0.as_str().into(),
        });
    }
    let unsafe_subtree:bool=db(conn.query_row(
        "WITH RECURSIVE tree(id,session_id,state,slot_held,root_id,execution_id,work_id) AS (SELECT id,session_id,state,slot_held,root_id,execution_id,work_id FROM assignments WHERE id=?1 UNION SELECT a.id,a.session_id,a.state,a.slot_held,a.root_id,a.execution_id,a.work_id FROM assignments a JOIN tree t ON a.parent_id=t.id)
         SELECT EXISTS(SELECT 1 FROM tree WHERE state IN ('Ready','Active','Waiting') OR slot_held=1)
         OR EXISTS(SELECT 1 FROM usage_attempts a LEFT JOIN usage_attempt_balances b ON b.attempt_id=a.attempt_id WHERE a.assignment_id IN (SELECT id FROM tree) AND coalesce(b.local_quiescent,0)=0)
         OR EXISTS(SELECT 1 FROM tree t JOIN usage_attempts a ON a.execution_root_id=t.root_id AND a.execution_id=t.execution_id AND a.work_id=t.work_id LEFT JOIN usage_attempt_balances b ON b.attempt_id=a.attempt_id WHERE a.assignment_id IS NULL AND coalesce(b.local_quiescent,0)=0)
         OR EXISTS(SELECT 1 FROM session_turns WHERE session_id IN (SELECT session_id FROM tree))
         OR EXISTS(SELECT 1 FROM owned_processes WHERE assignment_id IN (SELECT id FROM tree) AND state NOT IN ('Reaped','Failed'))
         OR EXISTS(SELECT 1 FROM queue_entries q JOIN operations o ON o.id=q.operation_id WHERE o.assignment_id IN (SELECT id FROM tree) AND q.state IN ('Ready','Claimed','Reconciling'))",
        [assignment.0.as_str()],|row|row.get(0),
    ))?;
    if unsafe_subtree {
        return Ok(false);
    }
    // DISTINCT is bounded by the operation-state vocabulary. Parsing and the canonical core
    // predicate keep this check aligned when states are added or their meaning changes.
    let mut statement=db(conn.prepare_cached(
        "WITH RECURSIVE tree(id) AS (SELECT id FROM assignments WHERE id=?1 UNION SELECT a.id FROM assignments a JOIN tree t ON a.parent_id=t.id) SELECT DISTINCT state FROM operations WHERE assignment_id IN (SELECT id FROM tree)",
    ))?;
    let states = db(db(
        statement.query_map([assignment.0.as_str()], |row| row.get::<_, String>(0))
    )?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    for state in states {
        let state = serde_json::from_value(serde_json::Value::String(state)).map_err(|_| {
            StorageError::unavailable(Unavailable::Corrupt, "unknown handoff operation state")
        })?;
        if possibly_dispatched(state) {
            return Ok(false);
        }
    }
    Ok(true)
}
