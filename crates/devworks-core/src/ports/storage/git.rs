//! Durable owned-worktree identity for Git services and host-bound invocations.
use super::StorageError;
use crate::protocol::data::GitOid;
use crate::protocol::{ActorId, AssignmentId, Id, OperationId, RootId};
use serde::{Deserialize, Serialize};

/// Physical directory witnesses captured after owned creation, not guessed from a pathname.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitWorktreeLocation {
    pub root: crate::authority::destructive::FileIdentity,
    pub git_dir: crate::authority::destructive::FileIdentity,
    pub common_dir: crate::authority::destructive::FileIdentity,
}

/// Ownership is retained after local cleanup. Current feature membership is resolved from
/// work records, not frozen into this historical ownership record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredGitWorktree {
    pub id: Id,
    pub repository_id: Id,
    pub work_id: Id,
    pub owner: ActorId,
    pub ownership_revision: i64,
    pub owner_root_id: Option<RootId>,
    pub owner_assignment_id: Option<AssignmentId>,
    pub registered_root_id: Id,
    pub path: String,
    pub branch: String,
    pub head: GitOid,
    pub revision: i64,
    pub removed: bool,
    pub location: Option<GitWorktreeLocation>,
    /// A reservation committed before a Git write. Unknown effects retain it and cannot be
    /// silently retried or overtaken by another write.
    pub pending_operation_id: Option<OperationId>,
    pub last_operation_id: Option<OperationId>,
    pub last_result: Option<serde_json::Value>,
}
impl StoredGitWorktree {
    pub fn validate(&self) -> Result<(), StorageError> {
        if self.revision < 1
            || self.ownership_revision < 1
            || self.path.is_empty()
            || self.path.contains('\0')
            || !self.branch.starts_with("refs/heads/")
            || self.branch.ends_with('/')
            || self
                .branch
                .contains([' ', '\\', '~', '^', ':', '?', '*', '[', '\r', '\n'])
            || self.branch.contains("..")
            || self.branch.contains("@{")
            || self.branch.contains("//")
            || self.branch.chars().any(char::is_control)
            || self.branch.split('/').any(|part| {
                part.is_empty()
                    || part.starts_with('.')
                    || part.ends_with('.')
                    || part.ends_with(".lock")
            })
        {
            return Err(StorageError::InvalidRecord("invalid owned worktree".into()));
        }
        self.head
            .validate()
            .map_err(|e| StorageError::InvalidRecord(e.into()))
    }
}
pub trait GitRead {
    fn git_worktree_for_branch(
        &self,
        repository: &Id,
        branch: &str,
    ) -> Result<Option<StoredGitWorktree>, StorageError>;
    fn git_pending_worktrees(
        &self,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredGitWorktree>, StorageError>;
    fn git_worktree(&self, id: &Id) -> Result<Option<StoredGitWorktree>, StorageError>;
    fn git_worktrees(
        &self,
        work: &Id,
        after: Option<&Id>,
        limit: u16,
    ) -> Result<Vec<StoredGitWorktree>, StorageError>;
}
pub trait GitRepository: GitRead {
    fn insert_git_worktree(&mut self, record: &StoredGitWorktree) -> Result<(), StorageError>;
    /// Resource/work/location identity is immutable. A current-authority, quiescent ownership
    /// transfer advances ownership_revision as well as the CAS revision; head observations do
    /// not change ownership_revision. The domain owner records the transfer history.
    fn update_git_worktree(
        &mut self,
        record: &StoredGitWorktree,
        expected_revision: i64,
    ) -> Result<(), StorageError>;
}
