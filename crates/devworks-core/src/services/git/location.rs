//! Resolve the physical repository and per-worktree Git directory, not just the saved path.
use super::*;
use crate::authority::destructive::{EntryKind, FileIdentity};
use crate::ports::storage::GitWorktreeLocation;
impl GitService {
    pub(super) fn resolve_location(
        &self,
        git: &Git<'_>,
        repository: &StoredRepository,
        tree: &StoredGitWorktree,
    ) -> Result<GitWorktreeLocation, GitFault> {
        let fs = self.config.filesystem.as_ref().ok_or_else(|| {
            GitFault::Helper(HelperFault::Refused(
                "physical Git identity is unavailable".into(),
            ))
        })?;
        let identity = |path: &str, root: bool| -> Result<FileIdentity, GitFault> {
            let resolved = fs
                .resolve(path, None)
                .map_err(|_| GitFault::SourceChanged)?;
            if root
                && (resolved.entry_kind != EntryKind::Directory
                    || !resolved.linked_ancestors.is_empty())
            {
                return Err(GitFault::SourceChanged);
            }
            resolved
                .target
                .filter(|t| t.is_directory)
                .and_then(|t| t.identity)
                .ok_or(GitFault::SourceChanged)
        };
        let (_, repository_common) = git.metadata_dirs(Path::new(&repository.location))?;
        let (git_dir, common_dir) = git.metadata_dirs(Path::new(&tree.path))?;
        let root = identity(&tree.path, true)?;
        if identity(&git.worktree_root(Path::new(&tree.path))?, true)? != root {
            return Err(GitFault::SourceChanged);
        }
        let common = identity(&common_dir, false)?;
        if common != identity(&repository_common, false)? {
            return Err(GitFault::SourceChanged);
        }
        Ok(GitWorktreeLocation {
            root,
            git_dir: identity(&git_dir, false)?,
            common_dir: common,
        })
    }
    pub(super) fn verify_target(&self, git: &Git<'_>, target: &Target) -> Result<(), GitFault> {
        if let Some(tree) = &target.worktree {
            let actual = self.resolve_location(git, &target.repository, tree)?;
            if tree.location.as_ref() != Some(&actual) {
                return Err(GitFault::SourceChanged);
            }
        }
        Ok(())
    }
}
