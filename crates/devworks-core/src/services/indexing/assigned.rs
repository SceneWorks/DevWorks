//! An assignment can address only the indexing layer of its currently owned checkout.

use super::{IndexEnv, discover, overlay, paths, section_config, section_key};
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::storage::*;
use crate::protocol::{
    DomainErrorCode, Id,
    results::{DomainDecision, OperationResult},
};
use crate::services::kernel::{Failure, Refusal};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) fn applies(ctx: &InvocationContext) -> bool {
    ctx.principal_kind() == PrincipalKind::Agent && ctx.assignment_id().is_some()
}

pub(crate) fn indexed_id(tree: &StoredGitWorktree) -> Id {
    discover::worktree_id(
        &tree.repository_id,
        &paths::native_location(&tree.path, cfg!(windows)),
    )
}

pub(crate) fn owned(
    git: &dyn GitRead,
    ctx: &InvocationContext,
) -> Result<Option<StoredGitWorktree>, StorageError> {
    let Some(bound) = ctx.worktree().filter(|_| applies(ctx)) else {
        return Ok(None);
    };
    Ok(git.git_worktree(bound.id())?.filter(|tree| {
        !tree.removed
            && tree.pending_operation_id.is_none()
            && &tree.owner == ctx.principal()
            && tree.owner_root_id.as_ref() == ctx.root_id()
            && tree.owner_assignment_id.as_ref() == ctx.assignment_id()
            && &tree.repository_id == bound.repository_id()
            && &tree.work_id == bound.work_id()
            && tree.ownership_revision == bound.ownership_revision()
    }))
}

fn live(
    assignment: Option<StoredAssignment>,
    session: Option<StoredSession>,
    ctx: &InvocationContext,
) -> bool {
    assignment
        .zip(session)
        .is_some_and(|(assignment, session)| {
            assignment.state.is_live()
                && Some(&assignment.root_id) == ctx.root_id()
                && Some(&assignment.session_id) == ctx.session_id()
                && !session.closed()
                && &session.agent_id == ctx.principal()
                && session.project_id.as_ref() == ctx.project_id()
                && session.root_id.as_ref() == ctx.root_id()
        })
}

pub(crate) fn refresh_target(
    git: &dyn GitRead,
    ctx: &InvocationContext,
    repository: &Id,
    requested: Option<&Id>,
    has_ref: bool,
) -> Result<Option<Id>, StorageError> {
    let Some(tree) = owned(git, ctx)? else {
        return Ok(None);
    };
    let indexed = indexed_id(&tree);
    Ok((!has_ref
        && &tree.repository_id == repository
        && requested.is_some_and(|id| id == &indexed || id == &tree.id))
    .then_some(indexed))
}

pub(crate) fn status_target(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
    repository: Option<&Id>,
    overlay: Option<&Id>,
) -> Result<(Id, Option<StoredOverlay>), StorageError> {
    let Some(tree) = owned(read.git(), ctx)? else {
        return Err(StorageError::ScopeDenied);
    };
    if repository.is_some_and(|id| id != &tree.repository_id) {
        return Err(StorageError::ScopeDenied);
    }
    let current = read
        .indexing()
        .overlay_for(&tree.repository_id, &indexed_id(&tree))?;
    if overlay.is_some_and(|id| !current.as_ref().is_some_and(|own| &own.id == id)) {
        return Err(StorageError::ScopeDenied);
    }
    Ok((tree.repository_id, current))
}

/// Queue the same derived index run as an explicit refresh and wait only within the graph
/// read's deadline. A failed capture never substitutes the published repository for this view.
pub(crate) fn ensure_current(
    env: &Arc<IndexEnv>,
    ctx: &InvocationContext,
    authorization: &crate::ports::Authorization,
    repository: &Id,
    worktree: &Id,
    deadline: i64,
    cancellation: Option<&Cancellation>,
) -> Result<(), Failure> {
    let unavailable = || {
        Refusal::new(
            DomainErrorCode::ViewUnavailable,
            "worktree_view",
            Some(worktree.clone()),
        )
    };
    let start = Instant::now();
    let allowed = Duration::from_millis(deadline.saturating_sub(env.now()).max(0) as u64);
    let stopped =
        || start.elapsed() >= allowed || cancellation.is_some_and(Cancellation::is_cancelled);
    let options = ReadOptions {
        deadline_ms: Some(deadline),
        cancellation: cancellation.cloned(),
    };
    let assignment = ctx
        .assignment_id()
        .cloned()
        .ok_or(StorageError::ScopeDenied)?;
    let session = ctx.session_id().cloned().ok_or(StorageError::ScopeDenied)?;
    let current = read_snapshot(&*env.store, &options, |read| {
        Ok(owned(read.git(), ctx)?.is_some_and(|tree| {
            &tree.repository_id == repository && &indexed_id(&tree) == worktree
        }) && live(
            read.runs().assignment(&assignment)?,
            read.sessions().session(&session)?,
            ctx,
        ))
    })?;
    if !current {
        return Err(StorageError::ScopeDenied.into());
    }
    loop {
        let observed = overlay::observe(env, ctx, repository, worktree, &stopped);
        if stopped() {
            return Err(StorageError::Cancelled.into());
        }
        let capture = observed
            .0
            .as_ref()
            .map_err(|_| unavailable())?
            .capture_digest
            .clone();
        let run = queue_current(
            env,
            ctx,
            authorization,
            (repository, worktree),
            (&assignment, &session),
            (deadline, cancellation),
            observed,
        )?;
        loop {
            if stopped() {
                return Err(StorageError::Cancelled.into());
            }
            let (state, current) = read_snapshot(&*env.store, &options, |read| {
                let state = read.indexing().run(&run)?.map(|run| run.state);
                let current = if state.as_deref() == Some(RUN_SUCCEEDED) {
                    read.indexing().overlay_for(repository, worktree)?
                } else {
                    None
                };
                Ok((state, current))
            })?;
            match state.as_deref() {
                Some(RUN_SUCCEEDED) => {
                    if current.is_some_and(|row| {
                        row.state == OVERLAY_READY && row.capture_digest == capture
                    }) {
                        return Ok(());
                    }
                    // Another request had already started this run. Check the checkout again
                    // and reuse its new view only if it contains the capture we observe.
                    break;
                }
                Some(RUN_QUEUED | RUN_RUNNING) => std::thread::sleep(Duration::from_millis(25)),
                _ => return Err(unavailable().into()),
            }
        }
    }
}

fn queue_current(
    env: &Arc<IndexEnv>,
    ctx: &InvocationContext,
    authorization: &crate::ports::Authorization,
    (repository, worktree): (&Id, &Id),
    (assignment, session): (&crate::protocol::AssignmentId, &Id),
    (deadline, cancellation): (i64, Option<&Cancellation>),
    observed: overlay::Observed,
) -> Result<Id, Failure> {
    let (env2, ctx2, auth, repo, tree, assignment, session) = (
        env.clone(),
        ctx.clone(),
        authorization.clone(),
        repository.clone(),
        worktree.clone(),
        assignment.clone(),
        session.clone(),
    );
    let mut write_options = WriteOptions::host_state();
    write_options.deadline_ms = Some(deadline);
    write_options.cancellation = cancellation.cloned();
    let run = run_write(&*env.store, write_options, move |tx| {
        let owned = owned(tx.git(), &ctx2)?
            .is_some_and(|current| current.repository_id == repo && indexed_id(&current) == tree);
        if !owned
            || !live(
                tx.runs().assignment(&assignment)?,
                tx.sessions().session(&session)?,
                &ctx2,
            )
            || !crate::authority::authorization_holds_in(tx, &ctx2, &auth)?
        {
            return Err(StorageError::ScopeDenied);
        }
        if tx.now_ms() >= deadline {
            return Err(StorageError::Cancelled);
        }
        let revision = section_config(tx.indexing(), &section_key(ctx2.project_id()))?.revision;
        match overlay::refresh(
            &env2,
            tx,
            &ctx2,
            &repo,
            overlay::Requested::Worktree {
                id: &tree,
                observed: Some(&observed),
            },
            revision,
        ) {
            Ok(decision) => Ok(Ok(decision)),
            Err(Failure::Refused(refusal)) => Ok(Err(refusal)),
            Err(Failure::Storage(error)) => Err(error),
        }
    })??;
    match run {
        DomainDecision::Applied {
            result: OperationResult::IndexingRefreshOverlay { value },
        }
        | DomainDecision::NoChange {
            result: OperationResult::IndexingRefreshOverlay { value },
        } => Ok(value.id),
        _ => Err(Refusal::new(
            DomainErrorCode::ViewUnavailable,
            "worktree_view",
            Some(worktree.clone()),
        )
        .into()),
    }
}
