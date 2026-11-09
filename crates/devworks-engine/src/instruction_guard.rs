//! Current, authorized repository guidance for provider contexts and send gates.
use devworks_core::{
    context::InvocationContext,
    ports::indexing::SourceTree,
    ports::storage::{ApplicationStore, StorageError, Unavailable},
    protocol::Id,
};
use std::sync::Arc;

pub(crate) struct InstructionGuard {
    store: Arc<dyn ApplicationStore>,
    source: Option<Arc<dyn SourceTree>>,
}
impl InstructionGuard {
    pub(crate) fn new(
        store: Arc<dyn ApplicationStore>,
        source: Option<Arc<dyn SourceTree>>,
    ) -> Self {
        Self { store, source }
    }

    pub(crate) fn verify(
        &self,
        ctx: &InvocationContext,
        context: &devworks_core::providers::context::ProviderContext,
        expected: Option<&[(Id, Vec<String>)]>,
    ) -> Result<(), StorageError> {
        context
            .validate()
            .map_err(|e| StorageError::InvalidRecord(format!("instruction context: {e:?}")))?;
        let (required, project_scope) = devworks_core::ports::storage::read_snapshot(
            self.store.as_ref(),
            &devworks_core::ports::storage::ReadOptions::default(),
            |read| {
                if read.stamp().store_id != *ctx.store_id()
                    || read.stamp().restore_epoch != ctx.restore_epoch().get()
                    || read.actors().actor_kind(ctx.principal())? != Some(ctx.principal_kind())
                {
                    return Err(StorageError::ScopeDenied);
                }
                let session_project = match ctx.session_id() {
                    Some(session_id) => read
                        .sessions()
                        .session(session_id)?
                        .ok_or(StorageError::ScopeDenied)?
                        .project_id
                        .is_some(),
                    None => false,
                };
                Ok((
                    ctx.worktree()
                        .map_or_else(Vec::new, |bound| vec![bound.repository_id().clone()]),
                    session_project || ctx.project_id().is_some(),
                ))
            },
        )?;
        if (project_scope || !required.is_empty() || !context.instruction_stamps.is_empty())
            && expected.is_none()
        {
            return Err(StorageError::unavailable(
                Unavailable::UnsupportedRuntime,
                "repository target selection is required before provider dispatch",
            ));
        }
        if required.len() > 64 {
            return Err(StorageError::InvalidRecord(
                "too many instruction repositories".into(),
            ));
        }
        if let Some(expected) = expected {
            if expected.len() != context.instruction_stamps.len() {
                return Err(StorageError::ScopeDenied);
            }
            let mut repositories = std::collections::BTreeSet::new();
            for (repository, paths) in expected {
                if !repositories.insert(repository.as_str()) {
                    return Err(StorageError::InvalidRecord(
                        "duplicate instruction repository".into(),
                    ));
                }
                let targets: Vec<String> = if paths.is_empty() {
                    vec![".".into()]
                } else {
                    paths
                        .iter()
                        .cloned()
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect()
                };
                if !context
                    .instruction_stamps
                    .iter()
                    .any(|stamp| &stamp.repository_id == repository && stamp.targets == targets)
                {
                    return Err(StorageError::ScopeDenied);
                }
            }
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
        let source = self.source.as_ref().ok_or_else(|| {
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
            source.as_ref(),
            std::path::Path::new(&root),
            repository_id.clone(),
            target_paths,
        )
        .map_err(|error| {
            StorageError::InvalidRecord(format!("instruction resolution: {error:?}"))
        })?;
        let check = devworks_core::providers::instructions::resolve(
            source.as_ref(),
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
