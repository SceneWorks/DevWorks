//! Current execution controls for authored guidance work. This is an eligibility check,
//! never an authority grant: each caller must reauthorize its own source, actor and scope.
use crate::{
    context::InvocationContext,
    ports::storage::*,
    protocol::{OperationId, RootId},
};
pub(crate) struct CapturedWorkControls {
    pub roots: Vec<(RootId, i64)>,
    pub assignment_generation: Option<i64>,
}
pub(crate) fn capture(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
) -> Result<CapturedWorkControls, StorageError> {
    let mut controls = Vec::new();
    if let Some(root) = ctx.root_id() {
        controls.push((
            root.clone(),
            tx.runs()
                .root_control(root)?
                .unwrap_or_else(|| StoredRootControl::initial(root.clone()))
                .stop_epoch,
        ));
        if let Some(work) = tx.work().work(&root.0)?
            && let Some(parent) = work.parent_feature_id
        {
            let parent = RootId(parent);
            if parent != *root {
                controls.push((
                    parent.clone(),
                    tx.runs()
                        .root_control(&parent)?
                        .unwrap_or_else(|| StoredRootControl::initial(parent.clone()))
                        .stop_epoch,
                ));
            }
        }
    }
    let assignment_generation = ctx
        .assignment_id()
        .map(|id| {
            tx.runs()
                .assignment(id)?
                .map(|a| a.generation)
                .ok_or(StorageError::ScopeDenied)
        })
        .transpose()?;
    Ok(CapturedWorkControls {
        roots: controls,
        assignment_generation,
    })
}
pub(crate) fn check(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    operation: &OperationId,
    roots: &[(RootId, i64)],
    assignment_generation: Option<i64>,
) -> Result<(), StorageError> {
    // An admitted effect never silently changes execution ancestry or assignment epoch.
    let current = capture(tx, ctx)?;
    if current.roots.as_slice() != roots || current.assignment_generation != assignment_generation {
        return Err(StorageError::ScopeDenied);
    }
    if !tx
        .runs()
        .blockers(&BlockerFilter::Operation(operation.clone()), None, 1)?
        .is_empty()
    {
        return Err(StorageError::ScopeDenied);
    }
    for (root, epoch) in roots {
        let control = tx
            .runs()
            .root_control(root)?
            .unwrap_or_else(|| StoredRootControl::initial(root.clone()));
        if control.stop_epoch != *epoch
            || control.desired != DesiredExecution::Enabled
            || !tx
                .runs()
                .blockers(&BlockerFilter::RootLevel(root.clone()), None, 1)?
                .is_empty()
        {
            return Err(StorageError::ScopeDenied);
        }
    }
    if let Some(id) = ctx.assignment_id() {
        let a = tx.runs().assignment(id)?.ok_or(StorageError::ScopeDenied)?;
        if Some(a.generation) != assignment_generation
            || a.root_id != *ctx.root_id().ok_or(StorageError::ScopeDenied)?
            || matches!(
                a.state,
                AssignmentState::Superseded | AssignmentState::Failed | AssignmentState::Completed
            )
            || !tx
                .runs()
                .blockers(&BlockerFilter::Assignment(id.clone()), None, 1)?
                .is_empty()
        {
            return Err(StorageError::ScopeDenied);
        }
    }
    Ok(())
}
