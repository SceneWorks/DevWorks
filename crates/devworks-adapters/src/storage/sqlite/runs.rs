//! SQLite run root control records: `root_controls`, `executions` (activations), `assignments`
//! and `blockers` (migration 1, extended by migration 11), `run_instructions` and
//! `session_turns` (migration 11), `stop_coverage` (migration 15), plus the read-only views run
//! control needs of `feature_plans`, `projects`, `work_items` and `queue_entries`.
use super::conn::db;
use super::repos::{Snapshot, Unit};
use devworks_core::ports::storage::*;
use devworks_core::protocol::{
    ActorId, AssignmentId, Id, OperationId, ProjectId, Revision, RevisionDimension, RootId,
};
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
fn text(v: &Value) -> String {
    v.to_string()
}
fn limit(l: u16) -> Result<i64, StorageError> {
    if l == 0 || l > MAX_PAGE {
        return Err(invalid(format!("limit must be 1..={MAX_PAGE}")));
    }
    Ok(i64::from(l))
}
fn rev(n: i64) -> Result<Revision, StorageError> {
    Revision::new(n.max(1)).map_err(|_| invalid("revision out of range"))
}
fn conflict(
    dimension: RevisionDimension,
    kind: &'static str,
    id: &Id,
    expected: i64,
    current: i64,
) -> StorageError {
    match (rev(expected), rev(current)) {
        (Ok(e), Ok(c)) => StorageError::conflict(dimension, kind, id.clone(), e, c),
        _ => invalid("revision out of range"),
    }
}
fn duplicate(kind: &'static str, id: &str) -> StorageError {
    StorageError::Duplicate {
        kind,
        id: id.to_owned(),
    }
}
/// A unique-index violation is a duplicate identity; any other failure is passed through.
fn unique(e: StorageError, kind: &'static str, id: &str) -> StorageError {
    match &e {
        StorageError::InvalidRecord(m) if m.contains("UNIQUE") => duplicate(kind, id),
        _ => e,
    }
}
fn bounded(field: &str, s: &str, max: usize) -> Result<(), StorageError> {
    if s.chars().count() > max || s.contains('\0') {
        return Err(invalid(format!(
            "{field} exceeds {max} characters or holds NUL"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Root controls

const CONTROL_COLUMNS: &str = "root_id,desired_state,revision,current_generation,restart_boot_id,instruction_revision,adopted_plan_revision,stop_epoch,stop_operation_id,reactivate,updated_at,stop_generation";
fn control_of(r: &Row<'_>) -> rusqlite::Result<ControlRaw> {
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
    ))
}
type ControlRaw = (
    String,
    String,
    i64,
    i64,
    Option<String>,
    i64,
    Option<i64>,
    i64,
    Option<String>,
    bool,
    i64,
    i64,
);
fn control(raw: ControlRaw) -> Result<StoredRootControl, StorageError> {
    let (root, desired, revision, generation, boot, instr, adopted, epoch, stop, react, at, sgen) =
        raw;
    Ok(StoredRootControl {
        root_id: RootId(id_of(root)?),
        desired: DesiredExecution::parse(&desired)
            .ok_or_else(|| corrupt(format!("stored desired state {desired}")))?,
        revision,
        generation,
        restart_boot_id: opt_id(boot)?,
        instruction_revision: instr,
        adopted_plan_revision: adopted,
        stop_epoch: epoch,
        stop_operation_id: opt_id(stop)?.map(OperationId),
        stop_generation: sgen,
        reactivate: react,
        updated_at_ms: at,
    })
}
fn controls(
    conn: &Connection,
    filter: &str,
    p: impl rusqlite::Params,
) -> Result<Vec<StoredRootControl>, StorageError> {
    let mut s = db(conn.prepare_cached(&format!(
        "SELECT {CONTROL_COLUMNS} FROM root_controls WHERE {filter}"
    )))?;
    db(db(s.query_map(p, control_of))?.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(control)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Activations

const ACTIVATION_COLUMNS: &str = "id,root_id,boot_id,generation,phase,purpose,owner_id,lease_until,owner_epoch,revision,attempts,ready_at,created_at,stop_cause,stop_requested_at,stop_acknowledged_at,stopped_at,outcome,detail,plan_revision";
type ActivationRaw = (
    (String, String, String, i64, String, String, Option<String>),
    (Option<i64>, i64, i64, i64, i64, i64),
    (
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<i64>,
    ),
);
fn activation_of(r: &Row<'_>) -> rusqlite::Result<ActivationRaw> {
    Ok((
        (
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
        ),
        (
            r.get(7)?,
            r.get(8)?,
            r.get(9)?,
            r.get(10)?,
            r.get(11)?,
            r.get(12)?,
        ),
        (
            r.get(13)?,
            r.get(14)?,
            r.get(15)?,
            r.get(16)?,
            r.get(17)?,
            r.get(18)?,
            r.get(19)?,
        ),
    ))
}
fn activation(raw: ActivationRaw) -> Result<StoredActivation, StorageError> {
    let ((id, root, boot, generation, phase, purpose, owner), b, c) = raw;
    let (lease, owner_epoch, revision, attempts, ready_at, created_at) = b;
    let (cause, requested, acknowledged, stopped, outcome, detail, plan) = c;
    Ok(StoredActivation {
        id: id_of(id)?,
        root_id: RootId(id_of(root)?),
        boot_id: id_of(boot)?,
        generation,
        phase: ActivationPhase::parse(&phase)
            .ok_or_else(|| corrupt(format!("stored activation phase {phase}")))?,
        purpose: ActivationPurpose::parse(&purpose)
            .ok_or_else(|| corrupt(format!("stored activation purpose {purpose}")))?,
        owner: opt_id(owner)?,
        lease_until_ms: lease,
        owner_epoch,
        revision,
        attempts,
        ready_at_ms: ready_at,
        created_at_ms: created_at,
        stop_cause: cause
            .map(|c| StopCause::parse(&c).ok_or_else(|| corrupt(format!("stored stop cause {c}"))))
            .transpose()?,
        stop_requested_at_ms: requested,
        stop_acknowledged_at_ms: acknowledged,
        stopped_at_ms: stopped,
        outcome: outcome
            .map(|o| {
                ActivationOutcome::parse(&o).ok_or_else(|| corrupt(format!("stored outcome {o}")))
            })
            .transpose()?,
        detail,
        plan_revision: plan,
    })
}
fn activations(
    conn: &Connection,
    filter: &str,
    p: impl rusqlite::Params,
) -> Result<Vec<StoredActivation>, StorageError> {
    let mut s = db(conn.prepare_cached(&format!(
        "SELECT {ACTIVATION_COLUMNS} FROM executions WHERE {filter}"
    )))?;
    db(db(s.query_map(p, activation_of))?.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(activation)
        .collect()
}
fn check_activation(a: &StoredActivation) -> Result<(), StorageError> {
    if a.revision < 1 || a.generation < 1 || a.owner_epoch < 0 || a.attempts < 0 {
        return Err(invalid(
            "activation revision, generation or counters out of range",
        ));
    }
    if a.owner.is_some() != a.lease_until_ms.is_some() {
        return Err(invalid("an activation owner always has a lease"));
    }
    if (a.phase == ActivationPhase::Stopped) != a.outcome.is_some() {
        return Err(invalid("exactly a stopped activation has an outcome"));
    }
    if let Some(d) = &a.detail {
        bounded("activation detail", d, 4096)?;
    }
    Ok(())
}
fn write_activation(
    conn: &Connection,
    a: &StoredActivation,
    sql: &str,
    expected: Option<i64>,
) -> Result<usize, rusqlite::Error> {
    let mut s = conn.prepare_cached(sql)?;
    let base = params![
        a.id.as_str(),
        a.root_id.0.as_str(),
        a.boot_id.as_str(),
        a.generation,
        a.phase.as_str(),
        a.purpose.as_str(),
        a.owner.as_ref().map(Id::as_str),
        a.lease_until_ms,
        a.owner_epoch,
        a.revision,
        a.attempts,
        a.ready_at_ms,
        a.created_at_ms,
        a.stop_cause.map(StopCause::as_str),
        a.stop_requested_at_ms,
        a.stop_acknowledged_at_ms,
        a.stopped_at_ms,
        a.outcome.map(ActivationOutcome::as_str),
        a.detail,
        a.plan_revision,
    ];
    match expected {
        None => s.execute(base),
        Some(e) => {
            let mut all: Vec<&dyn rusqlite::ToSql> = base.to_vec();
            all.push(&e);
            s.execute(all.as_slice())
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Blockers

const BLOCKER_COLUMNS: &str = "id,root_id,assignment_id,operation_id,cause,origin_id,origin_revision,state,revision,reason,clearance_json,created_at,cleared_at,cleared_by";
type BlockerRaw = (
    (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        String,
        i64,
    ),
    (
        String,
        i64,
        String,
        String,
        i64,
        Option<i64>,
        Option<String>,
    ),
);
fn blocker_of(r: &Row<'_>) -> rusqlite::Result<BlockerRaw> {
    Ok((
        (
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
        ),
        (
            r.get(7)?,
            r.get(8)?,
            r.get(9)?,
            r.get(10)?,
            r.get(11)?,
            r.get(12)?,
            r.get(13)?,
        ),
    ))
}
fn blocker(raw: BlockerRaw) -> Result<StoredBlocker, StorageError> {
    let ((id, root, assignment, operation, cause, origin, origin_rev), b) = raw;
    let (state, revision, reason, clearance, created, cleared, by) = b;
    Ok(StoredBlocker {
        id: id_of(id)?,
        root_id: opt_id(root)?.map(RootId),
        assignment_id: opt_id(assignment)?.map(AssignmentId),
        operation_id: opt_id(operation)?.map(OperationId),
        cause: HoldCause::parse(&cause)
            .ok_or_else(|| corrupt(format!("stored blocker cause {cause}")))?,
        origin_id: origin,
        origin_revision: origin_rev,
        state: match state.as_str() {
            "Active" => BlockerState::Active,
            "Cleared" => BlockerState::Cleared,
            other => return Err(corrupt(format!("stored blocker state {other}"))),
        },
        revision,
        reason,
        clearance: json_of(&clearance)?,
        created_at_ms: created,
        cleared_at_ms: cleared,
        cleared_by: opt_id(by)?.map(ActorId),
    })
}
fn blockers(
    conn: &Connection,
    filter: &str,
    p: impl rusqlite::Params,
) -> Result<Vec<StoredBlocker>, StorageError> {
    let mut s = db(conn.prepare_cached(&format!(
        "SELECT {BLOCKER_COLUMNS} FROM blockers WHERE {filter}"
    )))?;
    db(db(s.query_map(p, blocker_of))?.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(blocker)
        .collect()
}
fn state_name(s: BlockerState) -> &'static str {
    match s {
        BlockerState::Active => "Active",
        BlockerState::Cleared => "Cleared",
    }
}

// ---------------------------------------------------------------------------------------------
// Assignments

const ASSIGNMENT_COLUMNS: &str = "id,root_id,work_id,parent_id,session_id,execution_id,state,generation,slot_held,revision,checkpoint_json,scope_key,scope_digest,plan_revision,instruction_revision,created_at";
type AssignmentRaw = (
    (
        String,
        String,
        String,
        Option<String>,
        String,
        Option<String>,
        String,
        i64,
    ),
    (
        bool,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        i64,
        i64,
    ),
);
fn assignment_of(r: &Row<'_>) -> rusqlite::Result<AssignmentRaw> {
    Ok((
        (
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
            r.get(7)?,
        ),
        (
            r.get(8)?,
            r.get(9)?,
            r.get(10)?,
            r.get(11)?,
            r.get(12)?,
            r.get(13)?,
            r.get(14)?,
            r.get(15)?,
        ),
    ))
}
fn assignment(raw: AssignmentRaw) -> Result<StoredAssignment, StorageError> {
    let ((id, root, work, parent, session, execution, state, generation), b) = raw;
    let (slot, revision, checkpoint, key, digest, plan, instr, created) = b;
    Ok(StoredAssignment {
        id: AssignmentId(id_of(id)?),
        root_id: RootId(id_of(root)?),
        work_id: RootId(id_of(work)?),
        parent_id: opt_id(parent)?.map(AssignmentId),
        session_id: id_of(session)?,
        execution_id: opt_id(execution)?,
        state: AssignmentState::parse(&state)
            .ok_or_else(|| corrupt(format!("stored assignment state {state}")))?,
        generation,
        slot_held: slot,
        revision,
        checkpoint: checkpoint.as_deref().map(json_of).transpose()?,
        scope_key: key,
        scope_digest: digest,
        plan_revision: plan,
        instruction_revision: instr,
        created_at_ms: created,
    })
}
fn assignments(
    conn: &Connection,
    filter: &str,
    p: impl rusqlite::Params,
) -> Result<Vec<StoredAssignment>, StorageError> {
    let mut s = db(conn.prepare_cached(&format!(
        "SELECT {ASSIGNMENT_COLUMNS} FROM assignments WHERE {filter}"
    )))?;
    db(db(s.query_map(p, assignment_of))?.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(assignment)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Instructions and turns

const INSTRUCTION_COLUMNS: &str = "id,root_id,seq,actor_id,message,attachments_json,control_revision,instruction_revision,stage,received_at,acknowledged_at,acknowledged_by,incorporated_at,disposition_json,session_id,transcript_entry_id,revision";
type InstructionRaw = (
    (
        String,
        String,
        i64,
        String,
        String,
        String,
        i64,
        i64,
        String,
    ),
    (
        i64,
        Option<i64>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        i64,
    ),
);
fn instruction_of(r: &Row<'_>) -> rusqlite::Result<InstructionRaw> {
    Ok((
        (
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
            r.get(7)?,
            r.get(8)?,
        ),
        (
            r.get(9)?,
            r.get(10)?,
            r.get(11)?,
            r.get(12)?,
            r.get(13)?,
            r.get(14)?,
            r.get(15)?,
            r.get(16)?,
        ),
    ))
}
fn instruction(raw: InstructionRaw) -> Result<StoredInstruction, StorageError> {
    let ((id, root, seq, actor, message, attachments, control, instr, stage), b) = raw;
    let (received, acked, by, incorporated, disposition, session, entry, revision) = b;
    Ok(StoredInstruction {
        id: id_of(id)?,
        root_id: RootId(id_of(root)?),
        seq,
        actor: ActorId(id_of(actor)?),
        message,
        attachments: json_of(&attachments)?,
        control_revision: control,
        instruction_revision: instr,
        stage: InstructionStage::parse(&stage)
            .ok_or_else(|| corrupt(format!("stored instruction stage {stage}")))?,
        received_at_ms: received,
        acknowledged_at_ms: acked,
        acknowledged_by: opt_id(by)?,
        incorporated_at_ms: incorporated,
        disposition: disposition.as_deref().map(json_of).transpose()?,
        session_id: opt_id(session)?,
        transcript_entry_id: opt_id(entry)?,
        revision,
    })
}
fn instructions(
    conn: &Connection,
    filter: &str,
    p: impl rusqlite::Params,
) -> Result<Vec<StoredInstruction>, StorageError> {
    let mut s = db(conn.prepare_cached(&format!(
        "SELECT {INSTRUCTION_COLUMNS} FROM run_instructions WHERE {filter}"
    )))?;
    db(db(s.query_map(p, instruction_of))?.collect::<rusqlite::Result<Vec<_>>>())?
        .into_iter()
        .map(instruction)
        .collect()
}
fn turns(
    conn: &Connection,
    filter: &str,
    p: impl rusqlite::Params,
) -> Result<Vec<StoredTurn>, StorageError> {
    let mut s = db(conn.prepare_cached(&format!(
        "SELECT session_id,turn_id,assignment_id,execution_id,owner_epoch,boot_id,started_at FROM session_turns WHERE {filter}"
    )))?;
    let raw: Vec<(String, String, String, String, i64, String, i64)> =
        db(db(s.query_map(p, |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        }))?
        .collect::<rusqlite::Result<Vec<_>>>())?;
    raw.into_iter()
        .map(|(session, turn, a, e, epoch, boot, at)| {
            Ok(StoredTurn {
                session_id: id_of(session)?,
                turn_id: id_of(turn)?,
                assignment_id: AssignmentId(id_of(a)?),
                execution_id: id_of(e)?,
                owner_epoch: epoch,
                boot_id: id_of(boot)?,
                started_at_ms: at,
            })
        })
        .collect()
}
fn ids(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Vec<Id>, StorageError> {
    let mut s = db(conn.prepare_cached(sql))?;
    let raw: Vec<String> =
        db(db(s.query_map(p, |r| r.get(0)))?.collect::<rusqlite::Result<Vec<_>>>())?;
    raw.into_iter().map(id_of).collect()
}

const LIVE: &str = "phase<>'Stopped'";
const LIVE_ASSIGNMENT: &str = "state IN ('Ready','Active','Waiting')";

macro_rules! run_reads {
    ($t:ty) => {
        impl RunRead for $t {
            fn root_control(
                &self,
                root: &RootId,
            ) -> Result<Option<StoredRootControl>, StorageError> {
                Ok(controls(self.conn, "root_id=?1", [root.0.as_str()])?
                    .into_iter()
                    .next())
            }
            fn pending_stops(
                &self,
                after: Option<&RootId>,
                n: u16,
            ) -> Result<Vec<StoredRootControl>, StorageError> {
                let n = limit(n)?;
                controls(
                    self.conn,
                    "stop_operation_id IS NOT NULL AND root_id>?1 AND EXISTS(SELECT 1 FROM operations o WHERE o.id=stop_operation_id AND o.state NOT IN ('ObservedComplete','Failed','CancelledBeforeDispatch')) ORDER BY root_id LIMIT ?2",
                    params![after.map_or("", |r| r.0.as_str()), n],
                )
            }
            fn activation(&self, id: &Id) -> Result<Option<StoredActivation>, StorageError> {
                Ok(activations(self.conn, "id=?1", [id.as_str()])?
                    .into_iter()
                    .next())
            }
            fn live_activation(
                &self,
                root: &RootId,
            ) -> Result<Option<StoredActivation>, StorageError> {
                Ok(activations(
                    self.conn,
                    &format!("root_id=?1 AND {LIVE}"),
                    [root.0.as_str()],
                )?
                .into_iter()
                .next())
            }
            fn activations(
                &self,
                filter: &ActivationFilter,
                after: Option<&Id>,
                n: u16,
            ) -> Result<Vec<StoredActivation>, StorageError> {
                let n = limit(n)?;
                let after = after.map_or("", Id::as_str);
                match filter {
                    // Newest first: `after` is the generation-ordered keyset by ID only when
                    // absent; a root's activations page by descending generation.
                    ActivationFilter::Root(root) => activations(
                        self.conn,
                        "root_id=?1 AND (?2='' OR generation<(SELECT generation FROM executions WHERE id=?2)) ORDER BY generation DESC LIMIT ?3",
                        params![root.0.as_str(), after, n],
                    ),
                    ActivationFilter::Claimable { boot, now_ms } => activations(
                        self.conn,
                        "phase='Pending' AND boot_id=?1 AND ready_at<=?2 AND id>?3 ORDER BY id LIMIT ?4",
                        params![boot.as_str(), now_ms, after, n],
                    ),
                    ActivationFilter::LivePriorBoots(boot) => activations(
                        self.conn,
                        &format!("{LIVE} AND boot_id<>?1 AND id>?2 ORDER BY id LIMIT ?3"),
                        params![boot.as_str(), after, n],
                    ),
                    ActivationFilter::LiveOfBoot(boot) => activations(
                        self.conn,
                        &format!("{LIVE} AND boot_id=?1 AND id>?2 ORDER BY id LIMIT ?3"),
                        params![boot.as_str(), after, n],
                    ),
                }
            }
            fn blocker(&self, id: &Id) -> Result<Option<StoredBlocker>, StorageError> {
                Ok(blockers(self.conn, "id=?1", [id.as_str()])?.into_iter().next())
            }
            fn blockers(
                &self,
                filter: &BlockerFilter,
                after: Option<&Id>,
                n: u16,
            ) -> Result<Vec<StoredBlocker>, StorageError> {
                let n = limit(n)?;
                let after = after.map_or("", Id::as_str);
                let (cond, subject) = match filter {
                    BlockerFilter::Root(r) => ("root_id=?1", r.0.as_str()),
                    BlockerFilter::RootLevel(r) => (
                        "root_id=?1 AND assignment_id IS NULL AND operation_id IS NULL",
                        r.0.as_str(),
                    ),
                    BlockerFilter::Assignment(a) => ("assignment_id=?1", a.0.as_str()),
                    BlockerFilter::Operation(o) => ("operation_id=?1", o.0.as_str()),
                    BlockerFilter::RootLevelCause(c) => (
                        "cause=?1 AND root_id IS NOT NULL AND assignment_id IS NULL AND operation_id IS NULL",
                        c.as_str(),
                    ),
                };
                blockers(
                    self.conn,
                    &format!("{cond} AND state='Active' AND id>?2 ORDER BY id LIMIT ?3"),
                    params![subject, after, n],
                )
            }
            fn assignment(
                &self,
                id: &AssignmentId,
            ) -> Result<Option<StoredAssignment>, StorageError> {
                Ok(assignments(self.conn, "id=?1", [id.0.as_str()])?
                    .into_iter()
                    .next())
            }
            fn assignments(
                &self,
                root: &RootId,
                live_only: bool,
                after: Option<&AssignmentId>,
                n: u16,
            ) -> Result<Vec<StoredAssignment>, StorageError> {
                let n = limit(n)?;
                let live = if live_only { LIVE_ASSIGNMENT } else { "1=1" };
                assignments(
                    self.conn,
                    &format!("root_id=?1 AND {live} AND id>?2 ORDER BY id LIMIT ?3"),
                    params![root.0.as_str(), after.map_or("", |a| a.0.as_str()), n],
                )
            }
            fn instruction(&self, id: &Id) -> Result<Option<StoredInstruction>, StorageError> {
                Ok(instructions(self.conn, "id=?1", [id.as_str()])?
                    .into_iter()
                    .next())
            }
            fn instructions(
                &self,
                root: &RootId,
                after_seq: i64,
                n: u16,
            ) -> Result<Vec<StoredInstruction>, StorageError> {
                let n = limit(n)?;
                instructions(
                    self.conn,
                    "root_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3",
                    params![root.0.as_str(), after_seq, n],
                )
            }
            fn session_turn(&self, session: &Id) -> Result<Option<StoredTurn>, StorageError> {
                Ok(turns(self.conn, "session_id=?1", [session.as_str()])?
                    .into_iter()
                    .next())
            }
            fn turns_prior_boots(
                &self,
                boot: &Id,
                n: u16,
            ) -> Result<Vec<StoredTurn>, StorageError> {
                let n = limit(n)?;
                turns(
                    self.conn,
                    "boot_id<>?1 ORDER BY session_id LIMIT ?2",
                    params![boot.as_str(), n],
                )
            }
            fn feature_plan(
                &self,
                feature: &RootId,
            ) -> Result<Option<StoredPlanState>, StorageError> {
                let raw: Option<(i64, Option<String>, Option<i64>)> = db(self
                    .conn
                    .query_row(
                        "SELECT revision,approved_plan_json,pending_revision FROM feature_plans WHERE feature_id=?1",
                        [feature.0.as_str()],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional())?;
                let Some((revision, approved, pending_revision)) = raw else {
                    return Ok(None);
                };
                let approved_plan = approved
                    .as_deref()
                    .map(serde_json::from_str::<serde_json::Value>)
                    .transpose()
                    .map_err(|e| {
                        StorageError::unavailable(Unavailable::Corrupt, format!("stored plan: {e}"))
                    })?;
                let readiness = super::work::readiness_of(self.conn, &feature.0)?
                    .map(|r| (r.revision, r.body));
                Ok(Some(StoredPlanState {
                    revision,
                    approved: approved_plan.is_some(),
                    pending_revision,
                    approved_plan,
                    readiness,
                }))
            }
            fn work_parent(
                &self,
                work: &RootId,
            ) -> Result<Option<Option<RootId>>, StorageError> {
                let raw: Option<Option<String>> = db(self
                    .conn
                    .query_row(
                        "SELECT parent_feature_id FROM work_items WHERE id=?1",
                        [work.0.as_str()],
                        |r| r.get(0),
                    )
                    .optional())?;
                raw.map(|p| {
                    p.map(|p| {
                        Id::new(p).map(RootId).map_err(|_| {
                            StorageError::unavailable(Unavailable::Corrupt, "stored identifier")
                        })
                    })
                    .transpose()
                })
                .transpose()
            }
            fn open_prerequisites(&self, work: &RootId) -> Result<Vec<RootId>, StorageError> {
                Ok(super::work::open_prerequisites(self.conn, &work.0)?
                    .into_iter()
                    .map(RootId)
                    .collect())
            }
            fn project_lifecycle(
                &self,
                project: &ProjectId,
            ) -> Result<Option<String>, StorageError> {
                db(self
                    .conn
                    .query_row(
                        "SELECT lifecycle FROM projects WHERE id=?1",
                        [project.0.as_str()],
                        |r| r.get(0),
                    )
                    .optional())
            }
            fn project_roots(
                &self,
                project: &ProjectId,
                after: Option<&RootId>,
                n: u16,
            ) -> Result<Vec<RootId>, StorageError> {
                let n = limit(n)?;
                Ok(ids(
                    self.conn,
                    "SELECT id FROM work_items WHERE project_id=?1 AND (kind='feature' OR parent_feature_id IS NULL OR EXISTS(SELECT 1 FROM root_controls rc WHERE rc.root_id=work_items.id)) AND id>?2 ORDER BY id LIMIT ?3",
                    params![project.0.as_str(), after.map_or("", |r| r.0.as_str()), n],
                )?
                .into_iter()
                .map(RootId)
                .collect())
            }
            fn root_deliveries(
                &self,
                root: &RootId,
                after: Option<&Id>,
                n: u16,
            ) -> Result<Vec<Id>, StorageError> {
                let n = limit(n)?;
                ids(
                    self.conn,
                    "SELECT id FROM queue_entries WHERE root_id=?1 AND state IN ('Ready','Claimed','Reconciling','Failed') AND id>?2 ORDER BY id LIMIT ?3",
                    params![root.0.as_str(), after.map_or("", Id::as_str), n],
                )
            }
            fn stop_coverage(
                &self,
                op: &OperationId,
                after: Option<&RootId>,
                n: u16,
            ) -> Result<Vec<StoredStopCoverage>, StorageError> {
                coverage(
                    self.conn,
                    op,
                    after.map_or("", |r| r.0.as_str()),
                    limit(n)?,
                )
            }
        }
    };
}
fn coverage(
    conn: &Connection,
    op: &OperationId,
    after: &str,
    n: i64,
) -> Result<Vec<StoredStopCoverage>, StorageError> {
    let mut s = db(conn.prepare_cached(
        "SELECT root_id,generation,reactivate FROM stop_coverage WHERE operation_id=?1 AND root_id>?2 ORDER BY root_id LIMIT ?3",
    ))?;
    let raw: Vec<(String, i64, bool)> = db(db(s
        .query_map(params![op.0.as_str(), after, n], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        }))?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    raw.into_iter()
        .map(|(root, generation, reactivate)| {
            Ok(StoredStopCoverage {
                operation_id: op.clone(),
                root_id: RootId(id_of(root)?),
                generation,
                reactivate,
            })
        })
        .collect()
}
run_reads!(Snapshot<'_>);
run_reads!(Unit<'_>);

fn root_exists(conn: &Connection, root: &RootId) -> Result<(), StorageError> {
    let found: bool = db(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_items WHERE id=?1)",
        [root.0.as_str()],
        |r| r.get(0),
    ))?;
    if found {
        Ok(())
    } else {
        Err(StorageError::NotFound {
            kind: "root",
            id: root.0.as_str().to_owned(),
        })
    }
}

impl RunRepository for Unit<'_> {
    fn insert_root_control(&mut self, c: &StoredRootControl) -> Result<(), StorageError> {
        if c.revision < 1 || c.generation < 0 {
            return Err(invalid("root control revision or generation out of range"));
        }
        root_exists(self.conn, &c.root_id)?;
        if self.root_control(&c.root_id)?.is_some() {
            return Err(duplicate("root_control", c.root_id.0.as_str()));
        }
        db(self.conn.execute(
            &format!("INSERT INTO root_controls({CONTROL_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)"),
            params![
                c.root_id.0.as_str(),
                c.desired.as_str(),
                c.revision,
                c.generation,
                c.restart_boot_id.as_ref().map(Id::as_str),
                c.instruction_revision,
                c.adopted_plan_revision,
                c.stop_epoch,
                c.stop_operation_id.as_ref().map(|o| o.0.as_str()),
                c.reactivate,
                c.updated_at_ms,
                c.stop_generation,
            ],
        ))
        .map(|_| ())
        .map_err(|e| unique(e, "root_control", c.root_id.0.as_str()))
    }
    fn update_root_control(
        &mut self,
        c: &StoredRootControl,
        expected: i64,
    ) -> Result<(), StorageError> {
        let current = self
            .root_control(&c.root_id)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "root_control",
                id: c.root_id.0.as_str().to_owned(),
            })?;
        if current.revision != expected {
            return Err(conflict(
                RevisionDimension::Control,
                "root_control",
                &c.root_id.0,
                expected,
                current.revision,
            ));
        }
        if c.revision < expected || c.generation < current.generation {
            return Err(invalid(
                "a root control revision and generation never move backwards",
            ));
        }
        db(self.conn.execute(
            "UPDATE root_controls SET desired_state=?2,revision=?3,current_generation=?4,restart_boot_id=?5,instruction_revision=?6,adopted_plan_revision=?7,stop_epoch=?8,stop_operation_id=?9,reactivate=?10,updated_at=?11,stop_generation=?13 WHERE root_id=?1 AND revision=?12",
            params![
                c.root_id.0.as_str(),
                c.desired.as_str(),
                c.revision,
                c.generation,
                c.restart_boot_id.as_ref().map(Id::as_str),
                c.instruction_revision,
                c.adopted_plan_revision,
                c.stop_epoch,
                c.stop_operation_id.as_ref().map(|o| o.0.as_str()),
                c.reactivate,
                c.updated_at_ms,
                expected,
                c.stop_generation,
            ],
        ))
        .map(|_| ())
    }
    fn insert_activation(&mut self, a: &StoredActivation) -> Result<(), StorageError> {
        check_activation(a)?;
        if a.phase.is_live() && self.live_activation(&a.root_id)?.is_some() {
            return Err(duplicate("activation", a.root_id.0.as_str()));
        }
        write_activation(
            self.conn,
            a,
            &format!("INSERT INTO executions({ACTIVATION_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)"),
            None,
        )
        .map(|_| ())
        .map_err(|e| unique(super::conn::db_err(e), "activation", a.id.as_str()))
    }
    fn update_activation(
        &mut self,
        a: &StoredActivation,
        expected: i64,
    ) -> Result<(), StorageError> {
        check_activation(a)?;
        let current = self
            .activation(&a.id)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "activation",
                id: a.id.as_str().to_owned(),
            })?;
        if current.revision != expected {
            return Err(conflict(
                RevisionDimension::Control,
                "activation",
                &a.id,
                expected,
                current.revision,
            ));
        }
        if current.root_id != a.root_id
            || current.boot_id != a.boot_id
            || current.generation != a.generation
            || current.phase == ActivationPhase::Stopped
            || a.revision <= expected
            || a.owner_epoch < current.owner_epoch
        {
            return Err(invalid(
                "an activation keeps its root, boot and generation; a stopped one is final",
            ));
        }
        write_activation(
            self.conn,
            a,
            "UPDATE executions SET root_id=?2,boot_id=?3,generation=?4,phase=?5,purpose=?6,owner_id=?7,lease_until=?8,owner_epoch=?9,revision=?10,attempts=?11,ready_at=?12,created_at=?13,stop_cause=?14,stop_requested_at=?15,stop_acknowledged_at=?16,stopped_at=?17,outcome=?18,detail=?19,plan_revision=?20 WHERE id=?1 AND revision=?21",
            Some(expected),
        )
        .map(|_| ())
        .map_err(|e| unique(super::conn::db_err(e), "activation", a.id.as_str()))
    }
    fn insert_blocker(&mut self, b: &StoredBlocker) -> Result<(), StorageError> {
        bounded("blocker reason", &b.reason, 4096)?;
        if !b.clearance.is_object() || b.revision < 1 {
            return Err(invalid(
                "a blocker has an object clearance and a positive revision",
            ));
        }
        if b.root_id.is_none() && b.assignment_id.is_none() && b.operation_id.is_none() {
            return Err(invalid("a blocker names its root, assignment or operation"));
        }
        db(self.conn.execute(
            &format!("INSERT INTO blockers({BLOCKER_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)"),
            params![
                b.id.as_str(),
                b.root_id.as_ref().map(|r| r.0.as_str()),
                b.assignment_id.as_ref().map(|a| a.0.as_str()),
                b.operation_id.as_ref().map(|o| o.0.as_str()),
                b.cause.as_str(),
                b.origin_id,
                b.origin_revision,
                state_name(b.state),
                b.revision,
                b.reason,
                text(&b.clearance),
                b.created_at_ms,
                b.cleared_at_ms,
                b.cleared_by.as_ref().map(|a| a.0.as_str()),
            ],
        ))
        .map(|_| ())
        .map_err(|e| unique(e, "blocker", b.id.as_str()))
    }
    fn update_blocker(&mut self, b: &StoredBlocker, expected: i64) -> Result<(), StorageError> {
        bounded("blocker reason", &b.reason, 4096)?;
        let current = self.blocker(&b.id)?.ok_or_else(|| StorageError::NotFound {
            kind: "blocker",
            id: b.id.as_str().to_owned(),
        })?;
        if current.revision != expected {
            return Err(conflict(
                RevisionDimension::Blocker,
                "blocker",
                &b.id,
                expected,
                current.revision,
            ));
        }
        if current.state == BlockerState::Cleared
            || current.cause != b.cause
            || current.root_id != b.root_id
            || current.assignment_id != b.assignment_id
            || current.operation_id != b.operation_id
            || b.revision <= expected
            || !b.clearance.is_object()
        {
            return Err(invalid(
                "a blocker keeps its subject and cause; a cleared one is final",
            ));
        }
        db(self.conn.execute(
            "UPDATE blockers SET state=?2,revision=?3,reason=?4,clearance_json=?5,cleared_at=?6,cleared_by=?7 WHERE id=?1 AND revision=?8",
            params![
                b.id.as_str(),
                state_name(b.state),
                b.revision,
                b.reason,
                text(&b.clearance),
                b.cleared_at_ms,
                b.cleared_by.as_ref().map(|a| a.0.as_str()),
                expected,
            ],
        ))
        .map(|_| ())
    }
    fn insert_assignment(&mut self, a: &StoredAssignment) -> Result<(), StorageError> {
        if a.revision < 1 || a.generation < 0 || a.instruction_revision < 0 {
            return Err(invalid("assignment revision or counters out of range"));
        }
        if let Some(k) = &a.scope_key {
            bounded("scope key", k, 256)?;
        }
        db(self.conn.execute(
            &format!("INSERT INTO assignments({ASSIGNMENT_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)"),
            params![
                a.id.0.as_str(),
                a.root_id.0.as_str(),
                a.work_id.0.as_str(),
                a.parent_id.as_ref().map(|p| p.0.as_str()),
                a.session_id.as_str(),
                a.execution_id.as_ref().map(Id::as_str),
                a.state.as_str(),
                a.generation,
                a.slot_held,
                a.revision,
                a.checkpoint.as_ref().map(text),
                a.scope_key,
                a.scope_digest,
                a.plan_revision,
                a.instruction_revision,
                a.created_at_ms,
            ],
        ))
        .map(|_| ())
        .map_err(|e| unique(e, "assignment", a.id.0.as_str()))
    }
    fn update_assignment(
        &mut self,
        a: &StoredAssignment,
        expected: i64,
    ) -> Result<(), StorageError> {
        let current = self
            .assignment(&a.id)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "assignment",
                id: a.id.0.as_str().to_owned(),
            })?;
        if current.revision != expected {
            return Err(conflict(
                RevisionDimension::Work,
                "assignment",
                &a.id.0,
                expected,
                current.revision,
            ));
        }
        if current.root_id != a.root_id
            || current.work_id != a.work_id
            || current.parent_id != a.parent_id
            || !current.state.is_live()
            || a.revision <= expected
            || a.generation < current.generation
        {
            return Err(invalid(
                "an assignment keeps its root, work and parent; an ended one is final",
            ));
        }
        db(self.conn.execute(
            "UPDATE assignments SET session_id=?2,execution_id=?3,state=?4,generation=?5,slot_held=?6,revision=?7,checkpoint_json=?8,scope_key=?9,scope_digest=?10,plan_revision=?11,instruction_revision=?12 WHERE id=?1 AND revision=?13",
            params![
                a.id.0.as_str(),
                a.session_id.as_str(),
                a.execution_id.as_ref().map(Id::as_str),
                a.state.as_str(),
                a.generation,
                a.slot_held,
                a.revision,
                a.checkpoint.as_ref().map(text),
                a.scope_key,
                a.scope_digest,
                a.plan_revision,
                a.instruction_revision,
                expected,
            ],
        ))
        .map(|_| ())
    }
    fn insert_instruction(&mut self, i: &StoredInstruction) -> Result<(), StorageError> {
        bounded("instruction", &i.message, 65_536)?;
        if !i.attachments.is_array() || i.seq < 1 || i.revision < 1 {
            return Err(invalid(
                "instruction attachments, sequence or revision out of range",
            ));
        }
        db(self.conn.execute(
            &format!("INSERT INTO run_instructions({INSTRUCTION_COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)"),
            params![
                i.id.as_str(),
                i.root_id.0.as_str(),
                i.seq,
                i.actor.0.as_str(),
                i.message,
                text(&i.attachments),
                i.control_revision,
                i.instruction_revision,
                i.stage.as_str(),
                i.received_at_ms,
                i.acknowledged_at_ms,
                i.acknowledged_by.as_ref().map(Id::as_str),
                i.incorporated_at_ms,
                i.disposition.as_ref().map(text),
                i.session_id.as_ref().map(Id::as_str),
                i.transcript_entry_id.as_ref().map(Id::as_str),
                i.revision,
            ],
        ))
        .map(|_| ())
        .map_err(|e| unique(e, "instruction", i.id.as_str()))
    }
    fn update_instruction(
        &mut self,
        i: &StoredInstruction,
        expected: i64,
    ) -> Result<(), StorageError> {
        let current = self
            .instruction(&i.id)?
            .ok_or_else(|| StorageError::NotFound {
                kind: "instruction",
                id: i.id.as_str().to_owned(),
            })?;
        if current.revision != expected {
            return Err(conflict(
                RevisionDimension::Control,
                "instruction",
                &i.id,
                expected,
                current.revision,
            ));
        }
        let order = |s: InstructionStage| s as u8;
        if current.root_id != i.root_id
            || current.seq != i.seq
            || current.message != i.message
            || order(i.stage) < order(current.stage)
            || i.revision <= expected
        {
            return Err(invalid(
                "an instruction keeps its identity and text; its stages only advance",
            ));
        }
        db(self.conn.execute(
            "UPDATE run_instructions SET stage=?2,acknowledged_at=?3,acknowledged_by=?4,incorporated_at=?5,disposition_json=?6,session_id=?7,transcript_entry_id=?8,revision=?9 WHERE id=?1 AND revision=?10",
            params![
                i.id.as_str(),
                i.stage.as_str(),
                i.acknowledged_at_ms,
                i.acknowledged_by.as_ref().map(Id::as_str),
                i.incorporated_at_ms,
                i.disposition.as_ref().map(text),
                i.session_id.as_ref().map(Id::as_str),
                i.transcript_entry_id.as_ref().map(Id::as_str),
                i.revision,
                expected,
            ],
        ))
        .map(|_| ())
    }
    fn insert_turn(&mut self, t: &StoredTurn) -> Result<(), StorageError> {
        if self.session_turn(&t.session_id)?.is_some() {
            return Err(duplicate("session_turn", t.session_id.as_str()));
        }
        db(self.conn.execute(
            "INSERT INTO session_turns(session_id,turn_id,assignment_id,execution_id,owner_epoch,boot_id,started_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                t.session_id.as_str(),
                t.turn_id.as_str(),
                t.assignment_id.0.as_str(),
                t.execution_id.as_str(),
                t.owner_epoch,
                t.boot_id.as_str(),
                t.started_at_ms,
            ],
        ))
        .map(|_| ())
        .map_err(|e| unique(e, "session_turn", t.session_id.as_str()))
    }
    fn cover_stop(&mut self, c: &StoredStopCoverage) -> Result<(), StorageError> {
        if c.generation < 0 {
            return Err(invalid("stop coverage generation out of range"));
        }
        root_exists(self.conn, &c.root_id)?;
        let current: Option<i64> = db(self
            .conn
            .query_row(
                "SELECT generation FROM stop_coverage WHERE operation_id=?1 AND root_id=?2",
                params![c.operation_id.0.as_str(), c.root_id.0.as_str()],
                |r| r.get(0),
            )
            .optional())?;
        if current.is_some_and(|g| g > c.generation) {
            return Err(invalid(
                "a stop's coverage of a card never moves to an older generation",
            ));
        }
        db(self.conn.execute(
            "INSERT INTO stop_coverage(operation_id,root_id,generation,reactivate) VALUES(?1,?2,?3,?4) ON CONFLICT(operation_id,root_id) DO UPDATE SET generation=excluded.generation,reactivate=excluded.reactivate",
            params![
                c.operation_id.0.as_str(),
                c.root_id.0.as_str(),
                c.generation,
                c.reactivate,
            ],
        ))
        .map(|_| ())
    }
    fn delete_turn(&mut self, session: &Id, turn: &Id) -> Result<(), StorageError> {
        let n = db(self.conn.execute(
            "DELETE FROM session_turns WHERE session_id=?1 AND turn_id=?2",
            params![session.as_str(), turn.as_str()],
        ))?;
        if n == 1 {
            Ok(())
        } else {
            Err(StorageError::LeaseLost {
                kind: "session_turn",
                id: session.as_str().to_owned(),
            })
        }
    }
}
