//! Resolve the physical repository and per-worktree Git directory, not just the saved path.
use super::*;
use crate::authority::destructive::{EntryKind, FileIdentity, FilesystemResolver};
use crate::ports::storage::GitWorktreeLocation;
impl GitService {
    /// Git reports DOS paths on Windows while the ledger retains canonical verbatim paths.
    pub(super) fn same_worktree_path(&self, left: &Path, right: &Path) -> bool {
        let identity = |path: &Path| {
            self.config
                .filesystem
                .as_ref()?
                .resolve(path.to_str()?, None)
                .ok()?
                .target
                .filter(|t| t.is_directory)?
                .identity
        };
        if let (Some(a), Some(b)) = (identity(left), identity(right)) {
            return a == b;
        }
        // A reservation or prunable registration may not exist on disk yet.
        matches!(
            (crate::services::git::client::path_argument(left),
             crate::services::git::client::path_argument(right)),
            (Ok(a), Ok(b)) if Path::new(&a) == Path::new(&b)
        )
    }

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
        let (_, repository_common) = git.metadata_dirs(Path::new(&repository.location))?;
        let location = worktree_location(&**fs, git, &tree.path)?;
        if location.common_dir != directory_identity(&**fs, &repository_common, false)? {
            return Err(GitFault::SourceChanged);
        }
        Ok(location)
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

fn directory_identity(
    fs: &dyn FilesystemResolver,
    path: &str,
    root: bool,
) -> Result<FileIdentity, GitFault> {
    let resolved = fs
        .resolve(path, None)
        .map_err(|_| GitFault::SourceChanged)?;
    if root
        && (resolved.entry_kind != EntryKind::Directory || !resolved.linked_ancestors.is_empty())
    {
        return Err(GitFault::SourceChanged);
    }
    resolved
        .target
        .filter(|target| target.is_directory)
        .and_then(|target| target.identity)
        .ok_or(GitFault::SourceChanged)
}

fn worktree_location(
    fs: &dyn FilesystemResolver,
    git: &Git<'_>,
    path: &str,
) -> Result<GitWorktreeLocation, GitFault> {
    let dir = Path::new(path);
    let root = directory_identity(fs, path, true)?;
    let (git_dir, common_dir) = git.metadata_dirs(dir)?;
    if directory_identity(fs, &git.worktree_root(dir)?, true)? != root {
        return Err(GitFault::SourceChanged);
    }
    Ok(GitWorktreeLocation {
        root,
        git_dir: directory_identity(fs, &git_dir, false)?,
        common_dir: directory_identity(fs, &common_dir, false)?,
    })
}

/// A same-path replacement is outside custody, even if it points at the original Git metadata.
pub(crate) fn require_worktree_location(
    git: &Git<'_>,
    path: &str,
    expected: &GitWorktreeLocation,
) -> Result<(), GitFault> {
    let fs = git.runner.filesystem().ok_or(GitFault::Unqualified)?;
    if directory_identity(fs, path, true)? != expected.root
        || worktree_location(fs, git, path)? != *expected
    {
        return Err(GitFault::SourceChanged);
    }
    Ok(())
}
