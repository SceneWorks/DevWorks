//! Current, authorized repository guidance for provider contexts and send gates.
use crate::indexing::IndexSupervisor;
use devworks_core::{
    context::InvocationContext,
    ports::storage::{ApplicationStore, StorageError, Unavailable},
    protocol::Id,
};
use std::sync::Arc;

pub(crate) struct InstructionGuard {
    store: Arc<dyn ApplicationStore>,
    indexing: Option<Arc<IndexSupervisor>>,
}
impl InstructionGuard {
    pub(crate) fn new(
        store: Arc<dyn ApplicationStore>,
        indexing: Option<Arc<IndexSupervisor>>,
    ) -> Self {
        Self { store, indexing }
    }

    pub(crate) fn verify(
        &self,
        ctx: &InvocationContext,
        context: &devworks_core::providers::context::ProviderContext,
    ) -> Result<(), StorageError> {
        context
            .validate()
            .map_err(|e| StorageError::InvalidRecord(format!("instruction context: {e:?}")))?;
        let required = devworks_core::ports::storage::read_snapshot(
            self.store.as_ref(),
            &devworks_core::ports::storage::ReadOptions::default(),
            |read| {
                if read.stamp().store_id != *ctx.store_id()
                    || read.stamp().restore_epoch != ctx.restore_epoch().get()
                    || read.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
                {
                    return Err(StorageError::ScopeDenied);
                }
                Ok(ctx
                    .worktree()
                    .map_or_else(Vec::new, |bound| vec![bound.repository_id().clone()]))
            },
        )?;
        if required.len() > 64 {
            return Err(StorageError::InvalidRecord(
                "too many instruction repositories".into(),
            ));
        }
        for repository in &required {
            if !context
                .instruction_stamps
                .iter()
                .any(|stamp| &stamp.repository_id == repository)
            {
                return Err(StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    "repository guidance is not bound to this context",
                ));
            }
        }
        for stamp in &context.instruction_stamps {
            let current = self.resolve(ctx, &stamp.repository_id, &stamp.targets)?;
            if current.stamp() != *stamp {
                return Err(StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    "repository guidance changed before provider dispatch",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn resolve(
        &self,
        ctx: &InvocationContext,
        repository_id: &Id,
        target_paths: &[String],
    ) -> Result<devworks_core::providers::instructions::InstructionBundle, StorageError> {
        let indexing = self.indexing.as_ref().ok_or_else(|| {
            StorageError::unavailable(
                Unavailable::UnsupportedRuntime,
                "repository source reader unavailable",
            )
        })?;
        if let Some(bound) = ctx.worktree() {
            let fresh = devworks_core::services::git::context::bind_worktree(
                self.store.as_ref(),
                ctx.clone(),
                bound.id(),
            )?;
            if fresh.worktree() != Some(bound) {
                return Err(StorageError::ScopeDenied);
            }
        }
        let (root, repository_revision) = devworks_core::ports::storage::read_snapshot(
            self.store.as_ref(),
            &devworks_core::ports::storage::ReadOptions::default(),
            |read| {
                if read.stamp().store_id != *ctx.store_id()
                    || read.stamp().restore_epoch != ctx.restore_epoch().get()
                    || read.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
                {
                    return Err(StorageError::ScopeDenied);
                }
                if ctx.principal_kind() == devworks_core::context::PrincipalKind::Agent {
                    let binding = read
                        .authority()
                        .principal(ctx.principal())?
                        .ok_or(StorageError::ScopeDenied)?;
                    if !binding.active
                        || binding
                            .project_id
                            .as_ref()
                            .is_some_and(|project| ctx.project_id() != Some(project))
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                }
                let repository = read
                    .projects()
                    .repository(repository_id)?
                    .ok_or(StorageError::ScopeDenied)?;
                if !repository.primary || repository.lifecycle != "Active" {
                    return Err(StorageError::ScopeDenied);
                }
                if let Some(project) = ctx.project_id()
                    && !read.projects().members(project)?.contains(repository_id)
                {
                    return Err(StorageError::ScopeDenied);
                }
                if let Some(bound) = ctx.worktree() {
                    if bound.repository_id() != repository_id {
                        return Err(StorageError::ScopeDenied);
                    }
                    let worktree = read
                        .git()
                        .git_worktree(bound.id())?
                        .ok_or(StorageError::ScopeDenied)?;
                    if worktree.removed
                        || &worktree.repository_id != repository_id
                        || &worktree.work_id != bound.work_id()
                        || worktree.ownership_revision != bound.ownership_revision()
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                    Ok((worktree.path, repository.revision))
                } else {
                    if ctx.principal_kind() == devworks_core::context::PrincipalKind::Agent
                        && ctx.project_id().is_none()
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                    Ok((repository.location, repository.revision))
                }
            },
        )?;
        let bundle = devworks_core::providers::instructions::resolve(
            indexing.env().source.as_ref(),
            std::path::Path::new(&root),
            repository_id.clone(),
            target_paths,
        )
        .map_err(|error| {
            StorageError::InvalidRecord(format!("instruction resolution: {error:?}"))
        })?;
        let check = devworks_core::providers::instructions::resolve(
            indexing.env().source.as_ref(),
            std::path::Path::new(&root),
            repository_id.clone(),
            target_paths,
        )
        .map_err(|error| {
            StorageError::InvalidRecord(format!("instruction resolution: {error:?}"))
        })?;
        if check.revision != bundle.revision {
            return Err(StorageError::ViewUnavailable);
        }
        devworks_core::ports::storage::read_snapshot(
            self.store.as_ref(),
            &devworks_core::ports::storage::ReadOptions::default(),
            |read| {
                let current = read
                    .projects()
                    .repository(repository_id)?
                    .ok_or(StorageError::ScopeDenied)?;
                if read.stamp().store_id != *ctx.store_id()
                    || read.stamp().restore_epoch != ctx.restore_epoch().get()
                    || read.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
                    || current.revision != repository_revision
                    || (current.location != root && ctx.worktree().is_none())
                {
                    return Err(StorageError::ScopeDenied);
                }
                if let Some(project) = ctx.project_id()
                    && !read.projects().members(project)?.contains(repository_id)
                {
                    return Err(StorageError::ScopeDenied);
                }
                if ctx.principal_kind() == devworks_core::context::PrincipalKind::Agent {
                    let binding = read
                        .authority()
                        .principal(ctx.principal())?
                        .ok_or(StorageError::ScopeDenied)?;
                    if !binding.active
                        || binding
                            .project_id
                            .as_ref()
                            .is_some_and(|project| ctx.project_id() != Some(project))
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                }
                Ok(())
            },
        )?;
        if let Some(bound) = ctx.worktree() {
            let fresh = devworks_core::services::git::context::bind_worktree(
                self.store.as_ref(),
                ctx.clone(),
                bound.id(),
            )?;
            if fresh.worktree() != Some(bound) {
                return Err(StorageError::ScopeDenied);
            }
        }
        Ok(bundle)
    }
}
