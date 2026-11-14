//! Derive auxiliary inference ownership from the current session/turn ledger. This captures
//! existing facts only: canonical Usage admission still performs BeginDispatch and every gate.
use super::accounting::DispatchIdentity;
use crate::{
    context::{InvocationContext, PrincipalKind},
    ports::storage::*,
    protocol::Id,
};
#[derive(Clone, Debug)]
pub struct AuxiliaryOwnership {
    pub work_id: Option<Id>,
    pub job_run_id: Option<Id>,
    pub role: String,
    pub expected_context_epoch: Option<i64>,
    pub dispatch: Option<DispatchIdentity>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuxiliaryUnavailable {
    NoCurrentManagedInferenceOwner,
    SessionChanged,
}
#[derive(Debug)]
pub enum AuxiliaryError {
    Unavailable(AuxiliaryUnavailable),
    Storage(StorageError),
}
impl From<StorageError> for AuxiliaryError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}
fn missing() -> AuxiliaryError {
    AuxiliaryError::Unavailable(AuxiliaryUnavailable::NoCurrentManagedInferenceOwner)
}

pub fn ownership(
    read: &dyn ReadSnapshot,
    store: &StoreInfo,
    ctx: &InvocationContext,
) -> Result<AuxiliaryOwnership, AuxiliaryError> {
    if &read.stamp().store_id != ctx.store_id()
        || read.stamp().restore_epoch != ctx.restore_epoch().get()
        || read.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
    {
        return Err(StorageError::ScopeDenied.into());
    }
    let session = ctx
        .session_id()
        .map(|id| read.sessions().session(id))
        .transpose()?
        .flatten();
    if ctx.session_id().is_some()
        && session.as_ref().is_none_or(|s| {
            s.closed()
                || !crate::services::sessions::project_visible(ctx, s.project_id.as_ref())
                || (!matches!(
                    ctx.principal_kind(),
                    PrincipalKind::Human | PrincipalKind::Host
                ) && &s.agent_id != ctx.principal())
        })
    {
        return Err(AuxiliaryError::Unavailable(
            AuxiliaryUnavailable::SessionChanged,
        ));
    }
    let root = session.as_ref().and_then(|s| s.root_id.clone());
    if ctx.root_id().is_some_and(|id| root.as_ref() != Some(id)) {
        return Err(missing());
    }
    let mut work_id = root.as_ref().map(|id| id.0.clone());
    let dispatch = if let Some(root) = root.as_ref()
        && ctx.principal_kind() != PrincipalKind::Human
    {
        let session = ctx.session_id().ok_or_else(missing)?;
        let turn = read.runs().session_turn(session)?.ok_or_else(missing)?;
        let assignment = read
            .runs()
            .assignment(&turn.assignment_id)?
            .ok_or_else(missing)?;
        let activation = read
            .runs()
            .activation(&turn.execution_id)?
            .ok_or_else(missing)?;
        if Some(&turn.boot_id) != store.boot_id.as_ref()
            || activation.boot_id != turn.boot_id
            || assignment.root_id != *root
            || assignment.session_id != *session
            || assignment.execution_id.as_ref() != Some(&turn.execution_id)
            || !assignment.state.is_live()
            || ctx.assignment_id().is_some_and(|id| id != &assignment.id)
            || activation.root_id != assignment.root_id
            || activation.owner_epoch != turn.owner_epoch
        {
            return Err(missing());
        }
        work_id = Some(assignment.work_id.0);
        Some(DispatchIdentity {
            root_id: assignment.root_id,
            execution_id: turn.execution_id,
            owner_epoch: turn.owner_epoch,
            assignment_id: Some(assignment.id),
            assignment_generation: assignment.generation,
            turn_id: turn.turn_id,
            implementation: activation.purpose == ActivationPurpose::Implementation,
        })
    } else {
        None
    };
    let role = if matches!(
        ctx.principal_kind(),
        PrincipalKind::Human | PrincipalKind::Host
    ) {
        ctx.principal_kind().as_str().to_string()
    } else {
        let principal = read
            .authority()
            .principal(ctx.principal())?
            .ok_or(StorageError::ScopeDenied)?;
        if !principal.active
            || principal.kind != ctx.principal_kind()
            || principal
                .project_id
                .as_ref()
                .is_some_and(|p| ctx.project_id() != Some(p))
        {
            return Err(StorageError::ScopeDenied.into());
        }
        principal
            .role
            .map(|r| format!("{r:?}"))
            .ok_or(StorageError::ScopeDenied)?
    };
    Ok(AuxiliaryOwnership {
        work_id,
        job_run_id: ctx.job_run_id().cloned(),
        role,
        expected_context_epoch: session.as_ref().map(|s| s.context_epoch),
        dispatch,
    })
}
