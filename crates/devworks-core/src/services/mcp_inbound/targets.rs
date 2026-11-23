//! Resolve the native repository and exact index views of selected MCP requests.

use crate::context::InvocationContext;
use crate::operations::{OperationInput, code, documents, indexing, repositories};
use crate::ports::storage::{
    ArtifactRead, ArtifactRetention, ArtifactState, IndexRead, OVERLAY_UNAVAILABLE, ReadSnapshot,
    StorageError, StoredIndexRun, StoredOverlay,
};
use crate::protocol::data::{ArtifactCommand, DataScope, IndexCommand};
use crate::protocol::{Id, ProjectId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct WorktreeTarget {
    pub repository: Id,
    pub id: Id,
    /// A ref-based request must remain attached to a currently granted worktree.
    pub reference: Option<String>,
}

impl WorktreeTarget {
    pub fn holds(&self, index: &dyn IndexRead) -> Result<bool, StorageError> {
        Ok(index.worktrees(&self.repository)?.iter().any(|tree| {
            tree.worktree_id == self.id
                && self.reference.as_ref().is_none_or(|reference| {
                    tree.head_oid.as_ref() == Some(reference)
                        || tree.head_ref.as_deref().is_some_and(|full| {
                            full == reference
                                || full.strip_prefix("refs/heads/") == Some(reference.as_str())
                        })
                })
        }))
    }
}

#[derive(Default)]
pub(super) struct IndexTargets {
    pub repositories: Vec<Id>,
    pub worktrees: Vec<WorktreeTarget>,
    pub removal: Option<IndexRemoval>,
    pub document_run: Option<DocumentTarget>,
}

impl IndexTargets {
    fn repository(&mut self, repository: &Id) {
        self.repositories.push(repository.clone());
    }

    fn worktree(
        &mut self,
        read: &dyn IndexRead,
        repository: &Id,
        id: Option<&Id>,
        reference: Option<&str>,
        granted_worktrees: &[Id],
    ) -> Result<(), StorageError> {
        let Some(tree) = read.worktrees(repository)?.into_iter().find(|tree| {
            granted_worktrees.contains(&tree.worktree_id)
                && (id.is_some_and(|id| &tree.worktree_id == id)
                    || reference.is_some_and(|reference| {
                        tree.head_oid.as_deref() == Some(reference)
                            || tree.head_ref.as_deref().is_some_and(|full| {
                                full == reference
                                    || full.strip_prefix("refs/heads/") == Some(reference)
                            })
                    }))
        }) else {
            return Err(StorageError::ScopeDenied);
        };
        self.repository(repository);
        self.worktrees.push(WorktreeTarget {
            repository: repository.clone(),
            id: tree.worktree_id,
            reference: reference.map(str::to_owned),
        });
        Ok(())
    }

    fn overlay(
        &mut self,
        read: &dyn IndexRead,
        overlay: &StoredOverlay,
        granted_worktrees: &[Id],
    ) -> Result<(), StorageError> {
        self.worktree(
            read,
            &overlay.repository_id,
            overlay.worktree_id.as_ref(),
            if overlay.worktree_id.is_none() {
                overlay.head_oid.as_deref()
            } else {
                None
            },
            granted_worktrees,
        )
    }

    fn view(
        &mut self,
        read: &dyn IndexRead,
        view: &Id,
        repository: Option<&Id>,
        granted_worktrees: &[Id],
    ) -> Result<(), StorageError> {
        let view = read
            .view(view)?
            .filter(|view| repository.is_none_or(|id| id == &view.repository_id))
            .ok_or(StorageError::ScopeDenied)?;
        self.repository(&view.repository_id);
        if view.branch_id.is_some() || view.worktree_id.is_some() {
            let overlay = read
                .overlays(&view.repository_id)?
                .into_iter()
                .find(|overlay| overlay.view_id.as_ref() == Some(&view.view_id))
                .ok_or(StorageError::ScopeDenied)?;
            self.overlay(read, &overlay, granted_worktrees)?;
        }
        Ok(())
    }

    fn cancel(
        &mut self,
        read: &dyn IndexRead,
        ctx: &InvocationContext,
        named: StoredIndexRun,
        granted_worktrees: &[Id],
    ) -> Result<(), StorageError> {
        let runs = if named.id == named.batch_id {
            read.batch_runs(&named.id)?
        } else {
            vec![named]
        };
        for run in runs {
            if &run.requested_by != ctx.principal() || run.project_id.as_ref() != ctx.project_id() {
                return Err(StorageError::ScopeDenied);
            }
            self.repository(&run.repository_id);
            if let Some(overlay) = run.overlay_id {
                let overlay = read.overlay(&overlay)?.ok_or(StorageError::ScopeDenied)?;
                self.overlay(read, &overlay, granted_worktrees)?;
            }
        }
        Ok(())
    }

    pub fn resolve(
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &OperationInput,
        granted_worktrees: &[Id],
    ) -> Result<Self, StorageError> {
        let read = snapshot.indexing();
        let mut exact = Self::default();
        match input {
            OperationInput::Code(code::Input::SetTrust(input)) => {
                let IndexCommand::SetTrust {
                    repository_id,
                    view_id,
                    ..
                } = &input.command
                else {
                    return Err(StorageError::ScopeDenied);
                };
                exact.view(read, view_id, Some(repository_id), granted_worktrees)?;
            }
            OperationInput::Indexing(indexing::Input::RefreshOverlay(input)) => {
                let IndexCommand::RefreshOverlay {
                    repository_id,
                    worktree_id,
                    git_ref,
                    ..
                } = &input.command
                else {
                    return Err(StorageError::ScopeDenied);
                };
                exact.worktree(
                    read,
                    repository_id,
                    worktree_id.as_ref(),
                    git_ref.as_deref(),
                    granted_worktrees,
                )?;
            }
            OperationInput::Indexing(indexing::Input::RetireView(input)) => {
                exact.view(read, &input.view_id, None, granted_worktrees)?;
            }
            OperationInput::Indexing(indexing::Input::Cancel(input)) => {
                let IndexCommand::Cancel { operation_id } = &input.command else {
                    return Err(StorageError::ScopeDenied);
                };
                if let Some(run) = read.run(&operation_id.0)? {
                    exact.cancel(read, ctx, run, granted_worktrees)?;
                } else {
                    exact.document_run = Some(DocumentTarget::document_run(
                        snapshot,
                        ctx,
                        &operation_id.0,
                    )?);
                }
            }
            OperationInput::Repositories(repositories::Input::RemoveIndex(input)) => {
                exact.repository(&input.repository_id);
                let overlays = active_overlays(read, &input.repository_id)?;
                for overlay in &overlays {
                    exact.overlay(read, overlay, granted_worktrees)?;
                }
                exact.removal = Some(IndexRemoval {
                    repository: input.repository_id.clone(),
                    overlays: overlays.into_iter().map(|overlay| overlay.id).collect(),
                });
            }
            _ => {}
        }
        Ok(exact)
    }
}

fn active_overlays(
    read: &dyn IndexRead,
    repository: &Id,
) -> Result<Vec<StoredOverlay>, StorageError> {
    Ok(read
        .overlays(repository)?
        .into_iter()
        .filter(|overlay| overlay.state != OVERLAY_UNAVAILABLE)
        .collect())
}

/// Repository index removal also affects all existing overlays. Verify the same bound before
/// the writer enumerates those overlays and publishes any change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct IndexRemoval {
    repository: Id,
    overlays: Vec<Id>,
}
impl IndexRemoval {
    pub fn holds(&self, read: &dyn IndexRead) -> Result<bool, StorageError> {
        Ok(active_overlays(read, &self.repository)?
            .iter()
            .all(|overlay| self.overlays.contains(&overlay.id)))
    }
}

/// The durable audience of an existing artifact addressed by a document mutation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct DocumentTarget {
    id: Id,
    scope_revision: i64,
    owner_required: bool,
}

impl DocumentTarget {
    fn resolve(
        artifacts: &dyn ArtifactRead,
        project: &ProjectId,
        id: &Id,
        owner_required: bool,
    ) -> Result<Self, StorageError> {
        let artifact = artifacts
            .artifact(id)?
            .filter(|row| {
                row.state == ArtifactState::Live && row.retention == ArtifactRetention::Explicit
            })
            .ok_or(StorageError::ScopeDenied)?;
        let owns = artifact.scope.project() == Some(project);
        if !owns && (owner_required || !artifacts.shares(id)?.contains(project)) {
            return Err(StorageError::ScopeDenied);
        }
        Ok(Self {
            id: id.clone(),
            scope_revision: artifact.scope_revision,
            owner_required,
        })
    }

    /// The native document worker owns its IDs in the queue, not the repository index.
    /// Its original queued event independently attributes the exact document and project.
    fn document_run(
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        run: &Id,
    ) -> Result<Self, StorageError> {
        let entry = snapshot
            .queue()
            .entry(run)?
            .filter(|entry| entry.consumer == "document_index")
            .ok_or(StorageError::ScopeDenied)?;
        let project = ctx.project_id().ok_or(StorageError::ScopeDenied)?;
        let queued = snapshot
            .history()
            .event(&entry.event_id)?
            .filter(|event| {
                event.kind == "DocumentIndexQueued"
                    && event.aggregate_kind == "document_index_run"
                    && event.aggregate_id == run.as_str()
                    && event.project_id.as_ref() == Some(project)
                    && &event.actor == ctx.principal()
            })
            .ok_or(StorageError::ScopeDenied)?;
        let id = queued.payload["documentId"]
            .as_str()
            .and_then(|id| Id::new(id).ok())
            .ok_or(StorageError::ScopeDenied)?;
        Self::resolve(snapshot.artifacts(), project, &id, true)
    }

    pub fn current(
        &self,
        artifacts: &dyn ArtifactRead,
        project: &ProjectId,
    ) -> Result<Self, StorageError> {
        Self::resolve(artifacts, project, &self.id, self.owner_required)
    }

    pub fn holds(
        &self,
        artifacts: &dyn ArtifactRead,
        project: &ProjectId,
    ) -> Result<bool, StorageError> {
        match self.current(artifacts, project) {
            Ok(current) => Ok(&current == self),
            Err(StorageError::ScopeDenied) => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn covers(&self, previous: &Self) -> bool {
        self.id == previous.id && (!previous.owner_required || self.owner_required)
    }
}

pub(super) fn document_targets(
    read: &dyn ArtifactRead,
    project: &ProjectId,
    input: &OperationInput,
) -> Result<Vec<DocumentTarget>, StorageError> {
    let OperationInput::Documents(input) = input else {
        return Ok(Vec::new());
    };
    let id = match input {
        documents::Input::Import(input) => {
            if !matches!(&input.scope, DataScope::Project { project_id } if project_id == project) {
                return Err(StorageError::ScopeDenied);
            }
            let mut targets = vec![DocumentTarget::resolve(
                read,
                project,
                &input.source.artifact_id,
                false,
            )?];
            if let Some(replacement) = &input.replace_document_id {
                targets.push(DocumentTarget::resolve(read, project, replacement, true)?);
            }
            return Ok(targets);
        }
        documents::Input::Index(input) => &input.document_id,
        documents::Input::Retry(input) => &input.document_id,
        documents::Input::Share(_) | documents::Input::Move(_) => {
            return Err(StorageError::ScopeDenied);
        }
        documents::Input::DisableRetrieval(request) | documents::Input::RemoveIndex(request) => {
            match &request.command {
                ArtifactCommand::DisableRetrieval { artifact_id, .. }
                | ArtifactCommand::RemoveIndex { artifact_id, .. } => artifact_id,
                _ => return Err(StorageError::ScopeDenied),
            }
        }
        _ => return Ok(Vec::new()),
    };
    Ok(vec![DocumentTarget::resolve(read, project, id, true)?])
}
