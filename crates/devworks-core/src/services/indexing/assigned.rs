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
            crate::services::git::context::live_assignment(&assignment, &session, ctx)
        })
}

/// Check exact custody and live lineage against the snapshot that will publish a query.
pub(crate) fn live_custody(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
) -> Result<bool, StorageError> {
    let Some((assignment, session)) = ctx.assignment_id().zip(ctx.session_id()) else {
        return Ok(false);
    };
    let Some(tree) = owned(read.git(), ctx)? else {
        return Ok(false);
    };
    let work = read.work().work(&tree.work_id)?;
    Ok(live(
        read.runs().assignment(assignment)?,
        read.sessions().session(session)?,
        ctx,
    ) && work.is_some_and(|work| Some(&work.project_id) == ctx.project_id())
        && read
            .work()
            .work_repositories(&tree.work_id)?
            .contains(&tree.repository_id)
        && read
            .projects()
            .repository_projects(&tree.repository_id)?
            .iter()
            .any(|project| Some(project) == ctx.project_id()))
}

pub(crate) fn worktree_for_write(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
) -> Result<Option<StoredGitWorktree>, StorageError> {
    let Some((assignment, session)) = ctx.assignment_id().zip(ctx.session_id()) else {
        return Ok(None);
    };
    let Some(tree) = owned(tx.git(), ctx)? else {
        return Ok(None);
    };
    if !live(
        tx.runs().assignment(assignment)?,
        tx.sessions().session(session)?,
        ctx,
    ) || tx
        .work()
        .work(&tree.work_id)?
        .is_none_or(|work| Some(&work.project_id) != ctx.project_id())
        || !tx
            .work()
            .work_repositories(&tree.work_id)?
            .contains(&tree.repository_id)
        || !tx
            .projects()
            .repository_projects(&tree.repository_id)?
            .iter()
            .any(|project| Some(project) == ctx.project_id())
    {
        return Ok(None);
    }
    Ok(Some(tree))
}

pub(crate) fn capture_matches(observed: &overlay::Observed, checkout: &StoredGitWorktree) -> bool {
    checkout.location.is_some()
        && observed
            .0
            .as_ref()
            .ok()
            .and_then(|capture| capture.owned_checkout.as_ref())
            == Some(checkout)
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
    let checkout = read_snapshot(&*env.store, &options, |read| {
        if !live_custody(read, ctx)? {
            return Ok(None);
        }
        Ok(owned(read.git(), ctx)?
            .filter(|tree| &tree.repository_id == repository && &indexed_id(tree) == worktree))
    })?
    .ok_or_else(|| Refusal::new(DomainErrorCode::PermissionDenied, "worktree_custody", None))?;
    loop {
        let observed = overlay::observe(env, ctx, repository, worktree, &stopped);
        if stopped() {
            return Err(StorageError::Cancelled.into());
        }
        if !capture_matches(&observed, &checkout) {
            return Err(unavailable().into());
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
            &checkout,
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
                        let host = env.host_context(None);
                        let git = super::worker::git_for(env, &host, &stopped);
                        crate::services::git::location::require_worktree_location(
                            &git,
                            &checkout.path,
                            checkout.location.as_ref().ok_or_else(&unavailable)?,
                        )
                        .map_err(|_| unavailable())?;
                        if stopped() {
                            return Err(StorageError::Cancelled.into());
                        }
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
    checkout: &StoredGitWorktree,
    (deadline, cancellation): (i64, Option<&Cancellation>),
    observed: overlay::Observed,
) -> Result<Id, Failure> {
    let (env2, ctx2, auth, repo, tree, checkout) = (
        env.clone(),
        ctx.clone(),
        authorization.clone(),
        repository.clone(),
        worktree.clone(),
        checkout.clone(),
    );
    let mut write_options = WriteOptions::host_state();
    write_options.deadline_ms = Some(deadline);
    write_options.cancellation = cancellation.cloned();
    let run = run_write(&*env.store, write_options, move |tx| {
        if worktree_for_write(tx, &ctx2)?.as_ref() != Some(&checkout)
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
        } => {
            if let Some(notify) = env.run_queued.get() {
                notify();
            }
            Ok(value.id)
        }
        DomainDecision::NoChange {
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
