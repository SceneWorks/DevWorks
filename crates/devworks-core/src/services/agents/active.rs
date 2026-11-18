//! The one rule for "this assignment is running right now", used by every read and every command
//! of both message surfaces. An assignment is active when it is in the `Active` state and its
//! activation is `Running` in this process's boot with an unexpired lease; a caller that speaks for
//! it must also hold the matching session, principal, root and activation owner epoch.
use crate::context::InvocationContext;
use crate::ports::storage::{
    ActivationPhase, AssignmentState, ReadSnapshot, RunRead, StorageError, StoredActivation,
    StoredAssignment, StoredSession, WriteTransaction,
};
use crate::protocol::{AssignmentId, DomainErrorCode, Id, RootId};
use crate::services::failure::{Failure, Refusal};

/// The boot and the clock a lease is measured against.
pub(crate) struct Liveness {
    boot_id: Option<Id>,
    now_ms: i64,
}

impl Liveness {
    pub(crate) fn of_tx(tx: &dyn WriteTransaction) -> Self {
        Self {
            boot_id: tx.store().boot_id.clone(),
            now_ms: tx.now_ms(),
        }
    }
    pub(crate) fn of_snapshot(snapshot: &dyn ReadSnapshot) -> Self {
        let stamp = snapshot.stamp();
        Self {
            boot_id: stamp.boot_id.clone(),
            now_ms: stamp.now_ms,
        }
    }
}

fn denied(kind: &'static str) -> Failure {
    Refusal::new(DomainErrorCode::PermissionDenied, kind, None).into()
}

/// Whether `activation` is running in the current boot on an unexpired lease.
pub(crate) fn activation_is_live(live: &Liveness, activation: &StoredActivation) -> bool {
    activation.phase == ActivationPhase::Running
        && live.boot_id.as_ref() == Some(&activation.boot_id)
        && activation
            .lease_until_ms
            .is_some_and(|until| until > live.now_ms)
}

/// The activation of `assignment` when the assignment is `Active` and that activation is live.
pub(crate) fn live_activation(
    runs: &dyn RunRead,
    live: &Liveness,
    assignment: &StoredAssignment,
) -> Result<Option<StoredActivation>, StorageError> {
    if assignment.state != AssignmentState::Active {
        return Ok(None);
    }
    let Some(execution) = &assignment.execution_id else {
        return Ok(None);
    };
    Ok(runs
        .activation(execution)?
        .filter(|activation| activation_is_live(live, activation)))
}

/// The rows the proof reads, from a write transaction or a read snapshot.
pub(crate) trait Facts {
    fn live(&self) -> &Liveness;
    fn assignment(&mut self, id: &AssignmentId) -> Result<Option<StoredAssignment>, StorageError>;
    fn session(&mut self, id: &Id) -> Result<Option<StoredSession>, StorageError>;
    fn activation(&mut self, id: &Id) -> Result<Option<StoredActivation>, StorageError>;
}

pub(crate) struct TxFacts<'a>(pub &'a mut dyn WriteTransaction, Liveness);
impl<'a> TxFacts<'a> {
    pub(crate) fn new(tx: &'a mut dyn WriteTransaction) -> Self {
        let live = Liveness::of_tx(tx);
        Self(tx, live)
    }
}
impl Facts for TxFacts<'_> {
    fn live(&self) -> &Liveness {
        &self.1
    }
    fn assignment(&mut self, id: &AssignmentId) -> Result<Option<StoredAssignment>, StorageError> {
        self.0.runs().assignment(id)
    }
    fn session(&mut self, id: &Id) -> Result<Option<StoredSession>, StorageError> {
        self.0.sessions().session(id)
    }
    fn activation(&mut self, id: &Id) -> Result<Option<StoredActivation>, StorageError> {
        self.0.runs().activation(id)
    }
}

pub(crate) struct SnapshotFacts<'a>(pub &'a dyn ReadSnapshot, Liveness);
impl<'a> SnapshotFacts<'a> {
    pub(crate) fn new(snapshot: &'a dyn ReadSnapshot) -> Self {
        let live = Liveness::of_snapshot(snapshot);
        Self(snapshot, live)
    }
}
impl Facts for SnapshotFacts<'_> {
    fn live(&self) -> &Liveness {
        &self.1
    }
    fn assignment(&mut self, id: &AssignmentId) -> Result<Option<StoredAssignment>, StorageError> {
        self.0.runs().assignment(id)
    }
    fn session(&mut self, id: &Id) -> Result<Option<StoredSession>, StorageError> {
        self.0.sessions().session(id)
    }
    fn activation(&mut self, id: &Id) -> Result<Option<StoredActivation>, StorageError> {
        self.0.runs().activation(id)
    }
}

/// The assignment `ctx` speaks for on `root`, proved against current state: the assignment is
/// active in the root, the context's session and principal own it, and its activation is the one
/// the context carries, running in this boot on an unexpired lease.
pub(crate) fn caller_assignment(
    facts: &mut dyn Facts,
    ctx: &InvocationContext,
    root: &RootId,
) -> Result<AssignmentId, Failure> {
    let id = ctx.assignment_id().ok_or_else(|| denied("assignment"))?;
    let assignment = facts.assignment(id)?.ok_or_else(|| denied("assignment"))?;
    let session = facts
        .session(&assignment.session_id)?
        .ok_or_else(|| denied("session"))?;
    if assignment.root_id != *root
        || assignment.state != AssignmentState::Active
        || ctx.session_id() != Some(&assignment.session_id)
        || session.agent_id != *ctx.principal()
        || session.closed()
    {
        return Err(denied("assignment"));
    }
    let execution = assignment
        .execution_id
        .as_ref()
        .ok_or_else(|| denied("activation"))?;
    let activation = facts
        .activation(execution)?
        .ok_or_else(|| denied("activation"))?;
    if activation.root_id != *root
        || ctx.activation() != Some((execution, activation.owner_epoch))
        || !activation_is_live(facts.live(), &activation)
    {
        return Err(denied("activation"));
    }
    Ok(id.clone())
}
