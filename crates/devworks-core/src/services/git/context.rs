//! Host-only binding of an invocation to durable, currently owned Git work.
use crate::context::{BoundWorktree, InvocationContext, PrincipalKind};
use crate::ports::storage::{
    ApplicationStore, ReadOptions, ReadSnapshot, StorageError, StoredAssignment, StoredSession,
    read_snapshot,
};
use crate::protocol::Id;
use crate::services::work::model::WorkBrief;
/// Resolve host-selected identity, never a model-supplied path or revision. This grants no
/// operation authority. Every effect and observation must revalidate its ownership witness.
pub fn bind_worktree(
    store: &dyn ApplicationStore,
    context: InvocationContext,
    id: &Id,
) -> Result<InvocationContext, StorageError> {
    let info = store.info();
    if context.store_id() != &info.store_id || context.restore_epoch().get() != info.restore_epoch {
        return Err(StorageError::ScopeDenied);
    }
    let record = read_snapshot(store, &ReadOptions::default(), |s| {
        if s.actors().actor_kind(context.principal())? != Some(context.principal_kind()) {
            return Err(StorageError::ScopeDenied);
        }
        let tree = s.git().git_worktree(id)?.ok_or(StorageError::ScopeDenied)?;
        let work = s
            .work()
            .work(&tree.work_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let brief = WorkBrief::of(&work)?;
        let root_matches = context
            .root_id()
            .is_some_and(|r| r.0 == work.id || work.parent_feature_id.as_ref() == Some(&r.0));
        let delegate = matches!(
            context.principal_kind(),
            PrincipalKind::Human | PrincipalKind::Host
        ) || (context.principal_kind() == PrincipalKind::Agent
            && s.authority()
                .principal(context.principal())?
                .is_some_and(|principal| {
                    use crate::settings::catalog::AgentRole;
                    principal.role == Some(AgentRole::WorkspaceCoordinator)
                        || (principal.role == Some(AgentRole::FeatureCoordinator)
                            && work.parent_feature_id.as_ref().is_some_and(|feature| {
                                context.root_id().is_some_and(|root| &root.0 == feature)
                            }))
                }));
        let assigned = delegate
            || context.principal_kind() != PrincipalKind::Agent
            || (brief
                .assigned_agent_id
                .as_ref()
                .is_none_or(|a| a == &context.principal().0)
                && brief
                    .assignment_id
                    .as_ref()
                    .is_none_or(|a| Some(a) == context.assignment_id()));
        if tree.removed
            || (!delegate && &tree.owner != context.principal())
            || context.project_id() != Some(&work.project_id)
            || !root_matches
            || !assigned
            || !s
                .projects()
                .repository_projects(&tree.repository_id)?
                .contains(&work.project_id)
            || !s
                .work()
                .work_repositories(&work.id)?
                .contains(&tree.repository_id)
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(tree)
    })?;
    Ok(context.bind_worktree(BoundWorktree::resolved(
        record.id,
        record.repository_id,
        record.work_id,
        record.ownership_revision,
    )))
}

pub(crate) fn live_assignment(
    assignment: &StoredAssignment,
    session: &StoredSession,
    ctx: &InvocationContext,
) -> bool {
    assignment.state.is_live()
        && Some(&assignment.root_id) == ctx.root_id()
        && Some(&assignment.session_id) == ctx.session_id()
        && !session.closed()
        && &session.agent_id == ctx.principal()
        && session.project_id.as_ref() == ctx.project_id()
        && session.root_id.as_ref() == ctx.root_id()
}

/// Resolve a normal launcher context from current assignment custody, without trusting tool input.
pub fn assignment_worktree(
    read: &dyn ReadSnapshot,
    ctx: &InvocationContext,
) -> Result<Option<InvocationContext>, StorageError> {
    let Some(assignment_id) = ctx
        .assignment_id()
        .filter(|_| ctx.principal_kind() == PrincipalKind::Agent && ctx.worktree().is_none())
    else {
        return Ok(None);
    };
    if read.stamp().store_id != *ctx.store_id()
        || read.stamp().restore_epoch != ctx.restore_epoch().get()
    {
        return Err(StorageError::ScopeDenied);
    }
    let Some(assignment) = read.runs().assignment(assignment_id)? else {
        return Err(StorageError::ScopeDenied);
    };
    let Some(session) = read.sessions().session(&assignment.session_id)? else {
        return Err(StorageError::ScopeDenied);
    };
    if !live_assignment(&assignment, &session, ctx) {
        return Err(StorageError::ScopeDenied);
    }
    let work = read
        .work()
        .work(&assignment.work_id.0)?
        .ok_or(StorageError::ScopeDenied)?;
    let brief = WorkBrief::of(&work)?;
    if ctx.project_id() != Some(&work.project_id)
        || (work.id != assignment.root_id.0
            && work.parent_feature_id.as_ref() != Some(&assignment.root_id.0))
        || brief
            .assigned_agent_id
            .as_ref()
            .is_some_and(|id| id != &ctx.principal().0)
        || brief
            .assignment_id
            .as_ref()
            .is_some_and(|id| id != assignment_id)
    {
        return Err(StorageError::ScopeDenied);
    }
    let members = read.projects().members(&work.project_id)?;
    let repositories = read.work().work_repositories(&work.id)?;
    let mut after = None;
    let mut unique = None;
    loop {
        let page = read.git().git_worktrees(&work.id, after.as_ref(), 64)?;
        for row in &page {
            if row.removed
                || &row.owner != ctx.principal()
                || row.owner_root_id.as_ref() != ctx.root_id()
                || row.owner_assignment_id.as_ref() != ctx.assignment_id()
                || !members.contains(&row.repository_id)
                || !repositories.contains(&row.repository_id)
            {
                continue;
            }
            if unique.is_some() {
                return Ok(None);
            }
            unique = Some(BoundWorktree::resolved(
                row.id.clone(),
                row.repository_id.clone(),
                row.work_id.clone(),
                row.ownership_revision,
            ));
        }
        if page.len() < 64 {
            break;
        }
        after = page.last().map(|row| row.id.clone());
    }
    Ok(unique.map(|worktree| ctx.clone().bind_worktree(worktree)))
}

/// Trusted journal witness, never a tool input. Reconstructed only from committed host data.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SourceBinding {
    actor: crate::protocol::ActorId,
    kind: String,
    project: Option<crate::protocol::ProjectId>,
    root: Option<crate::protocol::RootId>,
    assignment: Option<crate::protocol::AssignmentId>,
    session: Option<Id>,
    worktree: Id,
    repository: Id,
    work: Id,
    ownership_revision: i64,
    restore_epoch: i64,
    #[serde(default)]
    prepared_revisions: Option<(i64, i64, i64, i64)>,
}
impl SourceBinding {
    pub fn of(ctx: &InvocationContext) -> Option<Self> {
        let tree = ctx.worktree()?;
        Some(Self {
            actor: ctx.principal().clone(),
            kind: ctx.principal_kind().as_str().to_string(),
            project: ctx.project_id().cloned(),
            root: ctx.root_id().cloned(),
            assignment: ctx.assignment_id().cloned(),
            session: ctx.session_id().cloned(),
            worktree: tree.id().clone(),
            repository: tree.repository_id().clone(),
            work: tree.work_id().clone(),
            ownership_revision: tree.ownership_revision(),
            restore_epoch: ctx.restore_epoch().get(),
            prepared_revisions: None,
        })
    }
    pub fn prepare(
        ctx: &InvocationContext,
        store: &dyn ApplicationStore,
    ) -> Result<Option<Self>, StorageError> {
        let Some(mut binding) = Self::of(ctx) else {
            return Ok(None);
        };
        let current = bind_worktree(store, ctx.clone(), &binding.worktree)?;
        if current.worktree() != ctx.worktree() {
            return Err(StorageError::ScopeDenied);
        }
        binding.prepared_revisions = Some(read_snapshot(store, &ReadOptions::default(), |s| {
            let tree = s
                .git()
                .git_worktree(&binding.worktree)?
                .ok_or(StorageError::ScopeDenied)?;
            let work = s
                .work()
                .work(&binding.work)?
                .ok_or(StorageError::ScopeDenied)?;
            let repo = s
                .projects()
                .repository(&binding.repository)?
                .ok_or(StorageError::ScopeDenied)?;
            let project = s
                .projects()
                .project(&work.project_id)?
                .ok_or(StorageError::ScopeDenied)?;
            Ok((
                tree.revision,
                work.revision,
                repo.revision,
                project.scope_revision,
            ))
        })?);
        Ok(Some(binding))
    }
    pub fn validate_in(
        &self,
        tx: &mut dyn crate::ports::storage::WriteTransaction,
    ) -> Result<bool, StorageError> {
        use crate::settings::catalog::AgentRole;
        let Some(tree) = tx.git().git_worktree(&self.worktree)? else {
            return Ok(false);
        };
        let Some(work) = tx.work().work(&self.work)? else {
            return Ok(false);
        };
        let Some(repo) = tx.projects().repository(&self.repository)? else {
            return Ok(false);
        };
        let Some(project) = tx.projects().project(&work.project_id)? else {
            return Ok(false);
        };
        let delegate = matches!(self.kind.as_str(), "Human" | "Host")
            || tx.authority().principal(&self.actor)?.is_some_and(|p| {
                p.role == Some(AgentRole::WorkspaceCoordinator)
                    || (p.role == Some(AgentRole::FeatureCoordinator)
                        && work
                            .parent_feature_id
                            .as_ref()
                            .is_some_and(|f| self.root.as_ref().is_some_and(|r| &r.0 == f)))
            });
        Ok(!tree.removed
            && tree.repository_id == self.repository
            && tree.work_id == self.work
            && tree.ownership_revision == self.ownership_revision
            && (delegate || tree.owner == self.actor)
            && self.project.as_ref() == Some(&work.project_id)
            && self
                .root
                .as_ref()
                .is_some_and(|r| r.0 == work.id || work.parent_feature_id.as_ref() == Some(&r.0))
            && self.prepared_revisions
                == Some((
                    tree.revision,
                    work.revision,
                    repo.revision,
                    project.scope_revision,
                ))
            && tx
                .projects()
                .repository_projects(&self.repository)?
                .contains(&work.project_id)
            && tx
                .work()
                .work_repositories(&self.work)?
                .contains(&self.repository))
    }
    pub fn context(&self, store: &dyn ApplicationStore) -> Result<InvocationContext, StorageError> {
        use crate::protocol::{Counter, Revision};
        let kind = PrincipalKind::parse(&self.kind).ok_or(StorageError::ScopeDenied)?;
        let info = store.info();
        if info.restore_epoch != self.restore_epoch {
            return Err(StorageError::ScopeDenied);
        }
        let mut ctx = crate::context::HostIngress::new(store)
            .context(
                self.actor.clone(),
                kind,
                Id::new("git-source").unwrap(),
                Id::new("git-source").unwrap(),
                Counter::new(self.restore_epoch).map_err(|_| StorageError::ScopeDenied)?,
                Revision::new(1).unwrap(),
            )
            .with_lineage(
                self.project.clone(),
                self.root.clone(),
                self.assignment.clone(),
            );
        if let Some(session) = &self.session {
            ctx = ctx.with_session(session.clone());
        }
        Ok(ctx.bind_worktree(BoundWorktree::resolved(
            self.worktree.clone(),
            self.repository.clone(),
            self.work.clone(),
            self.ownership_revision,
        )))
    }
}
