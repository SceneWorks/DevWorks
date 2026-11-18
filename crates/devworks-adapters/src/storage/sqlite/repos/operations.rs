//! The operation journal: operation and attempt rows and their state transitions.
use super::*;

pub(super) const OP_COLUMNS: &str = "id,root_id,assignment_id,capability,schema_version,args_digest,payload_json,state,revision,created_at";

pub(super) fn attempts(conn: &Connection, op: &str) -> Result<Vec<AttemptRecord>, StorageError> {
    let mut stmt = stmt(
        conn,
        "SELECT id,attempt_no,boot_id,execution_id,owner_generation,control_revision,policy_revision,state,observations_json FROM operation_attempts WHERE operation_id=?1 ORDER BY attempt_no",
    )?;
    type Raw = (
        String,
        i64,
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        String,
        String,
    );
    let rows: Vec<Raw> = db(db(stmt.query_map([op], |r| {
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
    }))?
    .collect())?;
    rows.into_iter()
        .map(|(id, no, boot, exec, gen_, ctrl, pol, state, obs)| {
            let observations = match json_of(&obs)? {
                Value::Array(a) => a,
                _ => return Err(corrupt("observations are not an array")),
            };
            Ok(AttemptRecord {
                id: AttemptId(id_of(id)?),
                attempt_no: no,
                boot_id: id_of(boot)?,
                binding: AttemptBinding {
                    execution_id: opt_id(exec)?.map(ExecutionId),
                    owner_generation: gen_,
                    control_revision: ctrl,
                    policy_revision: pol,
                },
                state: parse_state(&state)?,
                observations,
            })
        })
        .collect()
}

pub(super) type OpRaw = (
    String,
    Option<String>,
    Option<String>,
    String,
    i64,
    String,
    String,
    String,
    i64,
    i64,
);

pub(super) fn op_row(r: &Row<'_>) -> rusqlite::Result<OpRaw> {
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
}

pub(super) fn op_of(conn: &Connection, raw: OpRaw) -> Result<OperationRecord, StorageError> {
    let (
        id,
        root,
        assignment,
        capability,
        schema_version,
        args_digest,
        payload,
        state,
        rev,
        created,
    ) = raw;
    let attempts = attempts(conn, &id)?;
    Ok(OperationRecord {
        id: OperationId(id_of(id)?),
        root_id: opt_id(root)?.map(RootId),
        assignment_id: opt_id(assignment)?.map(AssignmentId),
        capability,
        schema_version,
        args_digest,
        payload: json_of(&payload)?,
        state: parse_state(&state)?,
        revision: rev,
        created_at_ms: created,
        attempts,
    })
}

pub(super) fn operation(
    conn: &Connection,
    id: &OperationId,
) -> Result<Option<OperationRecord>, StorageError> {
    let raw = db(stmt(
        conn,
        &format!("SELECT {OP_COLUMNS} FROM operations WHERE id=?1"),
    )?
    .query_row([id.0.as_str()], op_row)
    .optional())?;
    raw.map(|r| op_of(conn, r)).transpose()
}

pub(super) fn unresolved(
    conn: &Connection,
    max: u16,
) -> Result<Vec<OperationRecord>, StorageError> {
    let n = page_limit(max)?;
    let mut s = stmt(
        conn,
        &format!(
            "SELECT {OP_COLUMNS} FROM operations WHERE state NOT IN ('ObservedComplete','Failed','CancelledBeforeDispatch') ORDER BY id LIMIT ?1"
        ),
    )?;
    let raws: Vec<OpRaw> = db(db(s.query_map([n], op_row))?.collect())?;
    raws.into_iter().map(|r| op_of(conn, r)).collect()
}

pub(super) fn operations_for_capability(
    conn: &Connection,
    capability: &str,
    states: &[OperationState],
    after: Option<&OperationId>,
    max: u16,
) -> Result<Vec<OperationRecord>, StorageError> {
    let n = page_limit(max)?;
    bounded("capability", capability, 256)?;
    if states.is_empty() {
        return Ok(Vec::new());
    }
    let states = json!(states.iter().copied().map(state_name).collect::<Vec<_>>()).to_string();
    let mut query = stmt(
        conn,
        &format!(
            "SELECT {OP_COLUMNS} FROM operations WHERE capability=?1 AND state IN(SELECT value FROM json_each(?2)) AND (?3 IS NULL OR id>?3) ORDER BY id LIMIT ?4"
        ),
    )?;
    let raws: Vec<OpRaw> = db(db(query.query_map(
        params![capability, states, after.map(|id| id.0.as_str()), n],
        op_row,
    ))?
    .collect())?;
    raws.into_iter().map(|raw| op_of(conn, raw)).collect()
}

pub(super) fn state_name(s: OperationState) -> &'static str {
    use OperationState::*;
    match s {
        IntentRecorded => "IntentRecorded",
        Preflighted => "Preflighted",
        Dispatching => "Dispatching",
        ObservedComplete => "ObservedComplete",
        Failed => "Failed",
        OutcomeUnknown => "OutcomeUnknown",
        NeedsInput => "NeedsInput",
        RetryPending => "RetryPending",
        CancelledBeforeDispatch => "CancelledBeforeDispatch",
    }
}

pub(super) fn parse_state(s: &str) -> Result<OperationState, StorageError> {
    serde_json::from_value(Value::String(s.to_owned()))
        .map_err(|_| corrupt(format!("unknown operation state {s}")))
}

impl OperationJournal for Unit<'_> {
    fn record_intent(&mut self, op: NewOperation) -> Result<OperationRecord, StorageError> {
        bounded("capability", &op.capability, 256)?;
        bounded("args digest", &op.args_digest, 128)?;
        object("operation payload", &op.payload)?;
        if op.schema_version <= 0 {
            return Err(invalid("schema version must be positive"));
        }
        require(
            self.conn,
            "work_items",
            "root",
            op.root_id.as_ref().map(|r| r.0.as_str()),
        )?;
        require(
            self.conn,
            "assignments",
            "assignment",
            op.assignment_id.as_ref().map(|a| a.0.as_str()),
        )?;
        if operation(self.conn, &op.id)?.is_some() {
            return Err(StorageError::Duplicate {
                kind: "operation",
                id: op.id.0.as_str().to_owned(),
            });
        }
        db(stmt(self.conn,
            "INSERT INTO operations(id,root_id,assignment_id,capability,schema_version,args_digest,payload_json,state,revision,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,'IntentRecorded',1,?8)",
        )?
        .execute(params![
            op.id.0.as_str(),
            op.root_id.as_ref().map(|r| r.0.as_str()),
            op.assignment_id.as_ref().map(|a| a.0.as_str()),
            op.capability,
            op.schema_version,
            op.args_digest,
            text(&op.payload),
            self.now
        ]))?;
        self.load_op(&op.id)
    }

    fn begin_attempt(
        &mut self,
        id: &OperationId,
        expected_revision: i64,
        binding: AttemptBinding,
    ) -> Result<AttemptRecord, StorageError> {
        let op = self.load_op(id)?;
        self.op_cas(&op, expected_revision)?;
        if !matches!(
            op.state,
            OperationState::IntentRecorded | OperationState::RetryPending
        ) {
            return Err(invalid(format!(
                "cannot start an attempt from {}",
                state_name(op.state)
            )));
        }
        require(
            self.conn,
            "executions",
            "execution",
            binding.execution_id.as_ref().map(|e| e.0.as_str()),
        )?;
        let boot = self.boot()?.clone();
        let attempt_no = op.attempts.last().map_or(1, |a| a.attempt_no + 1);
        let attempt_id = self.ids.next_id(IdKind::Attempt);
        db(stmt(self.conn,
            "INSERT INTO operation_attempts(id,operation_id,attempt_no,boot_id,execution_id,owner_generation,control_revision,policy_revision,state,observations_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'Preflighted','[]')",
        )?
        .execute(params![
            attempt_id.as_str(),
            id.0.as_str(),
            attempt_no,
            boot.as_str(),
            binding.execution_id.as_ref().map(|e| e.0.as_str()),
            binding.owner_generation,
            binding.control_revision,
            binding.policy_revision
        ]))?;
        self.set_op_state(&op, OperationState::Preflighted)?;
        self.load_op(id)?
            .attempts
            .pop()
            .ok_or_else(|| corrupt("attempt vanished inside its transaction"))
    }

    fn transition(
        &mut self,
        id: &OperationId,
        expected_revision: i64,
        fence: Option<&AttemptFence>,
        to: OperationState,
        observation: Option<Value>,
    ) -> Result<OperationRecord, StorageError> {
        let op = self.load_op(id)?;
        self.op_cas(&op, expected_revision)?;
        if !transition_allowed(op.state, to) {
            return Err(invalid(format!(
                "illegal operation transition {} -> {}",
                state_name(op.state),
                state_name(to)
            )));
        }
        let attempt_bound = matches!(
            op.state,
            OperationState::Preflighted
                | OperationState::Dispatching
                | OperationState::OutcomeUnknown
                | OperationState::NeedsInput
        );
        let latest = op.latest_attempt();
        if matches!(
            op.state,
            OperationState::Preflighted | OperationState::Dispatching
        ) {
            let current = latest.map(AttemptRecord::fence);
            if fence.is_none() || fence != current.as_ref() {
                return Err(StorageError::LeaseLost {
                    kind: "operation_attempt",
                    id: id.0.as_str().to_owned(),
                });
            }
        }
        self.set_op_state(&op, to)?;
        match (attempt_bound, latest) {
            (true, Some(attempt)) => self.set_attempt(attempt, id, to, observation.as_ref())?,
            _ if observation.is_some() => {
                return Err(invalid(
                    "no current attempt to attribute the observation to",
                ));
            }
            _ => {}
        }
        self.load_op(id)
    }

    fn observe(
        &mut self,
        id: &OperationId,
        attempt_no: i64,
        observation: Value,
    ) -> Result<(), StorageError> {
        let op = self.load_op(id)?;
        if !op.attempts.iter().any(|a| a.attempt_no == attempt_no) {
            return Err(StorageError::NotFound {
                kind: "operation_attempt",
                id: format!("{}#{attempt_no}", id.0.as_str()),
            });
        }
        self.push_observation(id, attempt_no, &observation)
    }

    fn classify_interrupted(
        &mut self,
        max: u16,
    ) -> Result<Vec<(OperationId, OperationState)>, StorageError> {
        let n = page_limit(max)?;
        let boot = self.boot()?.clone();
        let ids: Vec<String> = {
            let mut s = stmt(
                self.conn,
                "SELECT o.id FROM operations o JOIN operation_attempts a ON a.operation_id=o.id AND a.attempt_no=(SELECT max(attempt_no) FROM operation_attempts WHERE operation_id=o.id) WHERE o.state IN ('Preflighted','Dispatching') AND a.boot_id<>?1 ORDER BY o.id LIMIT ?2",
            )?;
            db(db(s.query_map(params![boot.as_str(), n], |r| r.get(0)))?.collect())?
        };
        let mut out = Vec::new();
        for raw in ids {
            let id = OperationId(id_of(raw)?);
            let op = self.load_op(&id)?;
            let to = match op.state {
                OperationState::Preflighted => OperationState::RetryPending,
                _ => OperationState::OutcomeUnknown,
            };
            let attempt = op
                .latest_attempt()
                .ok_or_else(|| corrupt("interrupted operation without attempt"))?;
            self.set_op_state(&op, to)?;
            let note = json!({"recovery": state_name(to), "observedInBoot": boot.as_str(), "attemptBoot": attempt.boot_id.as_str()});
            self.set_attempt(attempt, &id, to, Some(&note))?;
            out.push((id, to));
        }
        Ok(out)
    }
}

impl Unit<'_> {
    pub(super) fn load_op(&self, id: &OperationId) -> Result<OperationRecord, StorageError> {
        operation(self.conn, id)?.ok_or_else(|| StorageError::NotFound {
            kind: "operation",
            id: id.0.as_str().to_owned(),
        })
    }

    pub(super) fn op_cas(&self, op: &OperationRecord, expected: i64) -> Result<(), StorageError> {
        if op.revision != expected {
            return Err(conflict_strict(
                RevisionDimension::Operation,
                "operation",
                op.id.0.as_str(),
                expected,
                op.revision,
            ));
        }
        Ok(())
    }

    pub(super) fn set_op_state(
        &self,
        op: &OperationRecord,
        to: OperationState,
    ) -> Result<(), StorageError> {
        let n = db(stmt(
            self.conn,
            "UPDATE operations SET state=?1,revision=revision+1 WHERE id=?2 AND revision=?3",
        )?
        .execute(params![state_name(to), op.id.0.as_str(), op.revision]))?;
        if n != 1 {
            return Err(corrupt("operation changed inside its transaction"));
        }
        Ok(())
    }

    pub(super) fn set_attempt(
        &self,
        attempt: &AttemptRecord,
        op: &OperationId,
        to: OperationState,
        observation: Option<&Value>,
    ) -> Result<(), StorageError> {
        db(stmt(
            self.conn,
            "UPDATE operation_attempts SET state=?1 WHERE operation_id=?2 AND attempt_no=?3",
        )?
        .execute(params![state_name(to), op.0.as_str(), attempt.attempt_no]))?;
        if let Some(o) = observation {
            self.push_observation(op, attempt.attempt_no, o)?;
        }
        Ok(())
    }

    pub(super) fn push_observation(
        &self,
        op: &OperationId,
        attempt_no: i64,
        observation: &Value,
    ) -> Result<(), StorageError> {
        object("observation", observation)?;
        db(stmt(self.conn,
            "UPDATE operation_attempts SET observations_json=json_insert(observations_json,'$[#]',json(?1)) WHERE operation_id=?2 AND attempt_no=?3",
        )?
        .execute(params![text(observation), op.0.as_str(), attempt_no]))?;
        Ok(())
    }
}
