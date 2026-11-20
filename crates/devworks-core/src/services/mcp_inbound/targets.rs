//! Resolve the native repository and exact index views of selected MCP requests.

use crate::context::InvocationContext;
use crate::operations::{OperationInput, code, indexing, repositories};
use crate::ports::storage::{
    IndexRead, OVERLAY_UNAVAILABLE, ReadSnapshot, StorageError, StoredOverlay,
};
use crate::protocol::Id;
use crate::protocol::data::IndexCommand;
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
        id: &Id,
        granted_worktrees: &[Id],
    ) -> Result<(), StorageError> {
        let named = read.run(id)?.ok_or(StorageError::ScopeDenied)?;
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
                exact.cancel(read, ctx, &operation_id.0, granted_worktrees)?;
            }
            OperationInput::Repositories(repositories::Input::RemoveIndex(input)) => {
                exact.repository(&input.repository_id);
                for overlay in read.overlays(&input.repository_id)? {
                    if overlay.state != OVERLAY_UNAVAILABLE {
                        exact.overlay(read, &overlay, granted_worktrees)?;
                    }
                }
            }
            _ => {}
        }
        Ok(exact)
    }
}
