//! Transfer an existing owned resource without moving/deleting its code or changing branch.
use super::*;
impl GitService {
    pub(super) fn attach_command(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        id: &Id,
    ) -> Result<(StoredGitWorktree, StoredRepository, StoredWork), Failure> {
        let mut tree = tx
            .git()
            .git_worktree(id)?
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, "worktree", id))?;
        if tree.owner == *ctx.principal() {
            return self.owned_command(tx, ctx, id);
        }
        let work = tx
            .work()
            .work(&tree.work_id)?
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, "work", &tree.work_id))?;
        let repository = tx
            .projects()
            .repository(&tree.repository_id)?
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, "repository", &tree.repository_id))?;
        self.check_work(tx, ctx, &work, &repository)?;
        if tree.removed || tree.pending_operation_id.is_some() {
            return Err(refuse(DomainErrorCode::Conflict, "git_ownership", id));
        }
        // A different agent cannot take an unassigned branch merely by naming its ID. The
        // current assignment must name this agent, this task, and its actual agent session.
        let brief = crate::services::work::model::WorkBrief::of(&work)?;
        if ctx.principal_kind() == PrincipalKind::Agent {
            if brief.assigned_agent_id.as_ref() != Some(&ctx.principal().0)
                || brief.assignment_id.as_ref() != ctx.assignment_id()
                || ctx.assignment_id().is_none()
            {
                return Err(refuse(
                    DomainErrorCode::PermissionDenied,
                    "git_assignment",
                    id,
                ));
            }
            let assignment = tx
                .runs()
                .assignment(ctx.assignment_id().expect("checked"))?
                .ok_or_else(|| refuse(DomainErrorCode::PermissionDenied, "git_assignment", id))?;
            if assignment.work_id.0 != work.id
                || !assignment.state.is_live()
                || Some(&assignment.session_id) != ctx.session_id()
            {
                return Err(refuse(
                    DomainErrorCode::PermissionDenied,
                    "git_assignment",
                    id,
                ));
            }
        } else if !matches!(
            ctx.principal_kind(),
            PrincipalKind::Human | PrincipalKind::Host
        ) {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                "git_assignment",
                id,
            ));
        }
        let mut roots = vec![RootId(work.id.clone())];
        for root in [
            tree.owner_root_id.clone(),
            work.parent_feature_id.clone().map(RootId),
        ]
        .into_iter()
        .flatten()
        {
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        for root in &roots {
            let mut after = None;
            loop {
                let assignments = tx
                    .runs()
                    .assignments(root, true, after.as_ref(), MAX_PAGE)?;
                if assignments
                    .iter()
                    .any(|a| a.work_id.0 == work.id && Some(&a.id) != ctx.assignment_id())
                {
                    return Err(refuse(
                        DomainErrorCode::Conflict,
                        "git_prior_assignment_live",
                        id,
                    ));
                }
                after = assignments.last().map(|a| a.id.clone());
                if assignments.len() < usize::from(MAX_PAGE) {
                    break;
                }
            }
        }
        let mut after = None;
        loop {
            let processes =
                tx.resources()
                    .processes(&ProcessFilter::Unsettled, after.as_ref(), MAX_PAGE)?;
            for process in &processes {
                if process.actor != tree.owner {
                    continue;
                }
                // The qualified browser owner writes only its isolated browser profile. It
                // is not a coding writer and does not need to close for a task handoff.
                if let Some(op) = &process.operation_id
                    && tx
                        .operations()
                        .operation(op)?
                        .is_some_and(|o| o.capability.starts_with("browser."))
                {
                    continue;
                }
                let assigned = match &process.assignment_id {
                    Some(id) => tx
                        .runs()
                        .assignment(id)?
                        .is_some_and(|a| a.work_id.0 == work.id),
                    None => false,
                };
                if assigned
                    || process
                        .root_id
                        .as_ref()
                        .is_some_and(|root| roots.contains(root))
                {
                    return Err(refuse(
                        DomainErrorCode::Conflict,
                        "git_prior_writer_not_quiesced",
                        id,
                    ));
                }
            }
            after = processes.last().map(|p| p.id.clone());
            if processes.len() < usize::from(MAX_PAGE) {
                break;
            }
        }
        tree.owner = ctx.principal().clone();
        tree.ownership_revision = tree.ownership_revision.checked_add(1).ok_or_else(|| {
            StorageError::InvalidRecord("Git ownership revision exhausted".into())
        })?;
        tree.owner_root_id = ctx.root_id().cloned();
        tree.owner_assignment_id = ctx.assignment_id().cloned();
        Ok((tree, repository, work))
    }
}

impl GitService {
    /// attach_command computes the proposed ownership in memory; it writes nothing. Reuse its
    /// complete assignment/process quiescence rules while retaining the current source binding.
    pub(crate) fn approval_attach_binding(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        id: &Id,
    ) -> Result<StoredGitWorktree, Failure> {
        self.attach_command(tx, ctx, id)?;
        tx.git()
            .git_worktree(id)?
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, "worktree", id))
    }
}
