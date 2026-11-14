//! `DestructivePreflight`: the one safety check typed filesystem, worktree cleanup, shell and
//! declared destructive MCP/package operations pass before dispatch (SEC-04/05).
//!
//! The input is a normalized [`EffectPlan`]; targets are resolved by the [`FilesystemResolver`]
//! port (relative paths against the saved working directory, `~`, bounded glob expansion, links
//! and reparse points, mount/volume roots), never by matching strings. Protected roots are
//! compared by stable filesystem identity, falling back to canonical paths only where the
//! platform adapter has no identity API. Volume/filesystem roots are always blocked; registered
//! primary repository roots, their ancestors, whole-content erasure and their `.git` metadata
//! are blocked unless a [`PrimaryDeletionPermit`] from the dedicated user confirmation names
//! exactly that repository; the user's home and the application data root are blocked for tool
//! deletions. Typed owned-worktree cleanup proves the exact owned, stopped, unused worktree and
//! cannot escape it or reach a primary checkout.
//!
//! An allowed decision is not a lasting permission: [`revalidate`] re-resolves every checked
//! target and the registry revision immediately before the step, and the
//! [`DestructiveExecutor`] verifies the same identities through no-follow handles before acting.
//! These are accident guards for supported paths, not interception of arbitrary user-level code.
use super::deletion::PrimaryDeletionPermit;
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::ports::IdKind;
use crate::ports::storage::{
    ApplicationStore, NewEvent, ReadOptions, RegisteredRepository, StorageError, WriteTransaction,
    read_snapshot,
};
use crate::protocol::{EventId, Id, StoreId};
use serde_json::json;
use std::path::{Path, PathBuf};

/// Most explicit targets one plan may expand to; more is `NeedsInput`, never a partial check.
pub const MAX_TARGETS: usize = 256;

/// Stable identity of a filesystem object (device/volume and file number).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct FileIdentity {
    pub volume: u64,
    pub file: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    Missing,
    File,
    Directory,
    Symlink,
    /// A Windows junction or other non-symlink reparse point.
    ReparsePoint,
    Other,
}

/// A located filesystem object: its path, identity and the identities of its ancestors
/// (nearest first). Identity is `None` where the platform adapter cannot provide one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located {
    pub path: PathBuf,
    pub identity: Option<FileIdentity>,
    pub ancestors: Vec<FileIdentity>,
    /// A filesystem/volume root or mount point.
    pub volume_root: bool,
    pub is_directory: bool,
}

/// One resolved target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPath {
    pub requested: String,
    /// The named directory entry (ancestors resolved, final component not followed).
    pub entry: Located,
    pub entry_kind: EntryKind,
    /// What the entry finally resolves to when followed; `None` when missing or dangling.
    pub target: Option<Located>,
    pub parent_traversal: bool,
    /// Requested ancestor components that are links or reparse points.
    pub linked_ancestors: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    TooMany {
        limit: usize,
    },
    Invalid(String),
    /// The adapter cannot preserve its identity rule for this path or platform.
    Unsupported(String),
    /// The target changed since it was checked.
    Changed(String),
    Io(String),
}

/// Platform path resolution (adapter).
pub trait FilesystemResolver: Send + Sync {
    fn resolve(
        &self,
        path: &str,
        working_directory: Option<&str>,
    ) -> Result<ResolvedPath, ResolveError>;
    /// Expands a glob into at most `limit` explicit paths.
    fn expand(
        &self,
        pattern: &str,
        working_directory: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>, ResolveError>;
    fn home(&self) -> Option<PathBuf>;
    /// The `gitdir:` pointer of a `.git` file (linked worktrees and submodules), unresolved.
    fn gitfile_target(&self, gitfile: &Path) -> Option<String>;
}

/// An owned task worktree as recorded by its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedWorktree {
    pub id: Id,
    pub repository_id: Id,
    pub location: String,
    pub stopped: bool,
    pub active_users: u32,
}

/// The protected-root registry snapshot a preflight runs against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedRoots {
    pub revision: String,
    pub repositories: Vec<RegisteredRepository>,
    pub worktrees: Vec<OwnedWorktree>,
    pub app_data_root: Option<String>,
    /// Skill folders (`~/.agents/skills`, `<data-root>/skills`): no command may create, change
    /// or remove anything inside them, so a skill changes only through skill saves and
    /// approvals ([`ProtectedRoots::with_skill_roots`]).
    pub skill_roots: Vec<String>,
    /// The store (and restore epoch) the registered roots were read from. Only roots read from
    /// a store ([`ProtectedRoots::read`]) can honor a primary deletion permit, and only one
    /// minted on that same store, epoch and registry revision.
    store: Option<(StoreId, i64)>,
}
impl ProtectedRoots {
    /// Reads the registered repositories from the store and binds the snapshot to that store's
    /// identity and restore epoch.
    pub fn read(
        store: &dyn ApplicationStore,
        worktrees: Vec<OwnedWorktree>,
        app_data_root: Option<String>,
    ) -> Result<Self, StorageError> {
        read_snapshot(store, &ReadOptions::default(), |s| {
            let repositories = s.authority().registered_repositories()?;
            let mut roots = Self::new(repositories, worktrees.clone(), app_data_root.clone());
            roots.store = Some((s.stamp().store_id.clone(), s.stamp().restore_epoch));
            Ok(roots)
        })
    }
    /// The store and restore epoch these roots were read from, if any.
    pub fn store(&self) -> Option<&(StoreId, i64)> {
        self.store.as_ref()
    }
    /// Builds the registry snapshot; its revision digests every registered root and worktree.
    pub fn new(
        repositories: Vec<RegisteredRepository>,
        worktrees: Vec<OwnedWorktree>,
        app_data_root: Option<String>,
    ) -> Self {
        let mut basis = String::new();
        for r in &repositories {
            basis.push_str(&format!(
                "r|{}|{}|{}\n",
                r.id.as_str(),
                r.location,
                r.revision
            ));
        }
        for w in &worktrees {
            basis.push_str(&format!(
                "w|{}|{}|{}|{}|{}\n",
                w.id.as_str(),
                w.repository_id.as_str(),
                w.location,
                w.stopped,
                w.active_users
            ));
        }
        basis.push_str(app_data_root.as_deref().unwrap_or(""));
        Self {
            revision: sha256_hex(basis.as_bytes()),
            repositories,
            worktrees,
            app_data_root,
            skill_roots: Vec::new(),
            store: None,
        }
    }
    /// Adds the skill folders no command may write; the revision covers them too.
    pub fn with_skill_roots(mut self, roots: impl IntoIterator<Item = String>) -> Self {
        self.skill_roots.extend(roots);
        let mut basis = self.revision.clone();
        for root in &self.skill_roots {
            basis.push_str(&format!("\ns|{root}"));
        }
        self.revision = sha256_hex(basis.as_bytes());
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DestructiveKind {
    /// Remove one directory entry without traversal (a link is removed, never followed).
    Unlink,
    /// Remove a directory tree.
    RemoveTree,
    /// Remove everything inside a directory, keeping the directory.
    EraseContents,
    /// Overwrite/truncate a file (follows the final link).
    Truncate,
    /// Move a tree away from its location (`mv`): for protection it destroys the location like
    /// a recursive removal. Never executed by the destructive executor.
    Move,
    /// A recognized consequential operation whose exact effect is not modeled (`git reset
    /// --hard`, `git gc`, `rsync` into a directory, a .NET delete call): refused when it touches
    /// a protected root like a recursive removal, otherwise held for input, never allowed.
    /// Never executed by the destructive executor.
    Consequential,
}
impl DestructiveKind {
    fn recursive(self) -> bool {
        matches!(
            self,
            Self::RemoveTree | Self::EraseContents | Self::Move | Self::Consequential
        )
    }
    fn follows(self) -> bool {
        !matches!(self, Self::Unlink)
    }
}

/// The intended scope the caller claims; claims are verified, never trusted.
#[derive(Debug)]
pub enum IntendedScope {
    Ordinary,
    OwnedWorktreeCleanup { worktree_id: Id },
    PrimaryRepositoryDeletion(PrimaryDeletionPermit),
}

/// A normalized destructive effect plan.
#[derive(Debug)]
pub struct EffectPlan {
    pub kind: DestructiveKind,
    pub targets: Vec<String>,
    pub working_directory: Option<String>,
    pub scope: IntendedScope,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BlockReason {
    FilesystemRoot,
    PrimaryRepository { repository_id: Id },
    RepositoryMetadata { repository_id: Id },
    WholeContentErasure,
    ProtectedUserDirectory,
    ApplicationData,
    SkillDirectory,
    WorktreeNotOwned,
    WorktreeInUse,
    EscapesScope,
    LinkEscape,
    PermitMismatch,
}
impl BlockReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FilesystemRoot => "FilesystemRoot",
            Self::PrimaryRepository { .. } => "PrimaryRepository",
            Self::RepositoryMetadata { .. } => "RepositoryMetadata",
            Self::WholeContentErasure => "WholeContentErasure",
            Self::ProtectedUserDirectory => "ProtectedUserDirectory",
            Self::ApplicationData => "ApplicationData",
            Self::SkillDirectory => "SkillDirectory",
            Self::WorktreeNotOwned => "WorktreeNotOwned",
            Self::WorktreeInUse => "WorktreeInUse",
            Self::EscapesScope => "EscapesScope",
            Self::LinkEscape => "LinkEscape",
            Self::PermitMismatch => "PermitMismatch",
        }
    }
    pub fn rationale(&self) -> &'static str {
        match self {
            Self::FilesystemRoot => "Drive, volume and filesystem roots are never deleted.",
            Self::PrimaryRepository { .. } => {
                "This would delete a registered repository. Only your dedicated repository deletion confirmation can do that; approvals and Full access cannot."
            }
            Self::RepositoryMetadata { .. } => {
                "This would destroy a registered repository's Git metadata."
            }
            Self::WholeContentErasure => {
                "This would erase every entry of a protected directory, which destroys it even though the directory survives."
            }
            Self::ProtectedUserDirectory => "This would delete your home directory.",
            Self::ApplicationData => {
                "DevWorks data changes only through application operations such as restore."
            }
            Self::SkillDirectory => {
                "Skill folders change only through skill saves and approvals, so a person reviews every change before it takes effect."
            }
            Self::WorktreeNotOwned => "This is not a worktree DevWorks owns.",
            Self::WorktreeInUse => "The worktree is still running or in use.",
            Self::EscapesScope => "The target resolves outside the claimed worktree.",
            Self::LinkEscape => "The path passes through a link, so its target could escape.",
            Self::PermitMismatch => {
                "The confirmation does not cover exactly this repository and target."
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InputNeeded {
    UnboundedExpansion,
    UnresolvedTarget,
    /// Declared or shell destructive intent whose targets could not be determined.
    UnresolvedDeclaredTarget,
    /// A shell argument depends on runtime expansion.
    UnresolvedExpansion,
    /// A working-directory change precedes a relative destructive target.
    UnresolvedWorkingDirectory,
    /// A construct (compound command, unmodeled wrapper, encoded or piped script) whose effect
    /// cannot be inspected before dispatch.
    OpaqueCommand,
    /// A protected root (registered repository, its Git directory, home, application data)
    /// could not be resolved, so nothing can be checked against it.
    UnresolvedProtectedRoot,
    /// A command contains a consequential program or verb that the structured analysis did not
    /// fully model (the lexical backstop), or a recognized consequential operation that is not
    /// refused but cannot be allowed.
    UnmodeledConsequentialCommand,
    /// Ref-rewriting or implicit Git pushes need the Git owner's concrete destination checks.
    GitPushOwnerRequired,
}
impl InputNeeded {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnboundedExpansion => "UnboundedExpansion",
            Self::UnresolvedTarget => "UnresolvedTarget",
            Self::UnresolvedDeclaredTarget => "UnresolvedDeclaredTarget",
            Self::UnresolvedExpansion => "UnresolvedExpansion",
            Self::UnresolvedWorkingDirectory => "UnresolvedWorkingDirectory",
            Self::OpaqueCommand => "OpaqueCommand",
            Self::UnresolvedProtectedRoot => "UnresolvedProtectedRoot",
            Self::UnmodeledConsequentialCommand => "UnmodeledConsequentialCommand",
            Self::GitPushOwnerRequired => "GitPushOwnerRequired",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreflightVerdict {
    Allowed,
    Blocked(BlockReason),
    NeedsInput(InputNeeded),
}

/// How a checked target was classified.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TargetClass {
    Ordinary,
    OwnedWorktree,
    /// Confirmed primary repository deletion.
    ConfirmedPrimaryRepository,
    /// Nothing exists at the target.
    NoTarget,
}

/// A target as checked; only a preflight decision constructs one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedTarget {
    requested: String,
    entry: Located,
    entry_kind: EntryKind,
    target_identity: Option<FileIdentity>,
    parent_traversal: bool,
    class: TargetClass,
}
impl CheckedTarget {
    pub fn requested(&self) -> &str {
        &self.requested
    }
    pub fn path(&self) -> &Path {
        &self.entry.path
    }
    pub fn entry_kind(&self) -> EntryKind {
        self.entry_kind
    }
    pub fn entry_identity(&self) -> Option<FileIdentity> {
        self.entry.identity
    }
    pub fn target_identity(&self) -> Option<FileIdentity> {
        self.target_identity
    }
    pub fn parent_traversal(&self) -> bool {
        self.parent_traversal
    }
    pub fn class(&self) -> TargetClass {
        self.class
    }
}

/// The preflight outcome with its evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreflightDecision {
    pub verdict: PreflightVerdict,
    pub kind: DestructiveKind,
    /// The target that decided a refusal.
    pub offending: Option<String>,
    targets: Vec<CheckedTarget>,
    pub registry_revision: String,
    /// Identities of every protected root and Git directory not covered by a deletion permit.
    /// The executor re-checks them inside the tree at execution time.
    protected: Vec<FileIdentity>,
}
impl PreflightDecision {
    pub fn protected_identities(&self) -> &[FileIdentity] {
        &self.protected
    }
    /// Targets of an allowed decision; nothing for a refusal.
    pub fn allowed_targets(&self) -> Option<&[CheckedTarget]> {
        (self.verdict == PreflightVerdict::Allowed).then_some(&self.targets[..])
    }
    pub fn rationale(&self) -> &'static str {
        match &self.verdict {
            PreflightVerdict::Allowed => "Allowed by the destructive-operation guard.",
            PreflightVerdict::Blocked(r) => r.rationale(),
            PreflightVerdict::NeedsInput(InputNeeded::UnboundedExpansion) => {
                "The targets expand to too many entries to check; name them explicitly."
            }
            PreflightVerdict::NeedsInput(_) => {
                "The actual targets could not be determined before dispatch."
            }
        }
    }
}

/// A protected object resolved for one preflight.
struct Protected {
    located: Located,
}
/// Resolves a protected location; `Err` when it cannot be resolved at all (relative, invalid,
/// unreadable), which fails the preflight closed. A registered location that is currently
/// missing stays protected by its path.
fn locate(fs: &dyn FilesystemResolver, location: &str) -> Result<Protected, ()> {
    let r = fs.resolve(location, None).map_err(|_| ())?;
    Ok(Protected {
        located: r.target.unwrap_or(r.entry),
    })
}

/// A registered repository: its root and every Git directory that belongs to it (the `.git`
/// entry itself, and the real Git directory a `.git` link or gitfile points to).
struct Repository {
    id: Id,
    root: Protected,
    git_dirs: Vec<Protected>,
}
fn repository(fs: &dyn FilesystemResolver, r: &RegisteredRepository) -> Result<Repository, ()> {
    let root = locate(fs, &r.location)?;
    let dot = root.located.path.join(".git");
    let dot_text = dot.to_string_lossy().into_owned();
    let entry = fs.resolve(&dot_text, None).map_err(|_| ())?;
    let mut git_dirs = Vec::new();
    if let Some(target) = &entry.target
        && entry.entry_kind == EntryKind::Symlink
    {
        git_dirs.push(Protected {
            located: target.clone(),
        });
    }
    if entry.entry_kind == EntryKind::File
        && let Some(pointer) = fs.gitfile_target(&dot)
    {
        let base = root.located.path.to_string_lossy().into_owned();
        let real = fs.resolve(&pointer, Some(&base)).map_err(|_| ())?;
        git_dirs.push(Protected {
            located: real.target.unwrap_or(real.entry),
        });
    }
    git_dirs.push(Protected {
        located: entry.entry,
    });
    Ok(Repository {
        id: r.id.clone(),
        root,
        git_dirs,
    })
}

/// A Windows path folded for comparison: lowercase, without the verbatim prefix a resolved
/// (canonical) path carries, so `\\?\C:\x` and a plain `C:\x` written by a command compare equal.
fn folded(p: &Path) -> PathBuf {
    let text = p.to_string_lossy().to_lowercase();
    let plain = match text.strip_prefix(r"\\?\unc\") {
        Some(rest) => format!(r"\\{rest}"),
        None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned(),
    };
    PathBuf::from(plain)
}
fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        folded(a) == folded(b)
    } else {
        a == b
    }
}
fn path_within(inner: &Path, outer: &Path) -> bool {
    if cfg!(windows) {
        folded(inner).starts_with(folded(outer))
    } else {
        inner.starts_with(outer)
    }
}
/// `a` and `b` are the same object.
fn same(a: &Located, b: &Located) -> bool {
    match (a.identity, b.identity) {
        (Some(x), Some(y)) => x == y,
        _ => same_path(&a.path, &b.path),
    }
}
/// `a` is a strict ancestor of `b`.
fn ancestor_of(a: &Located, b: &Located) -> bool {
    match a.identity {
        Some(id) if b.identity.is_some() => b.ancestors.contains(&id),
        _ => path_within(&b.path, &a.path) && !same_path(&a.path, &b.path),
    }
}
/// `a` is `b` or inside it.
fn within(a: &Located, b: &Located) -> bool {
    same(a, b) || ancestor_of(b, a)
}

fn has_wildcard(s: &str) -> bool {
    // The `?` of a Windows verbatim prefix (`\\?\C:\…`) is not a glob.
    let s = s.strip_prefix(r"\\?\").unwrap_or(s);
    s.contains(['*', '?', '['])
}

/// Runs the preflight. Refusals are total: one blocked or unresolved target refuses the plan.
pub fn preflight(
    fs: &dyn FilesystemResolver,
    roots: &ProtectedRoots,
    plan: &EffectPlan,
) -> PreflightDecision {
    let mut protected_ids: Vec<FileIdentity> = Vec::new();
    let decide = |verdict, offending: Option<&str>, targets, protected: Vec<FileIdentity>| {
        PreflightDecision {
            verdict,
            kind: plan.kind,
            offending: offending.map(str::to_owned),
            targets,
            registry_revision: roots.revision.clone(),
            protected,
        }
    };
    let unresolved = |offending: &str| {
        decide(
            PreflightVerdict::NeedsInput(InputNeeded::UnresolvedProtectedRoot),
            Some(offending),
            Vec::new(),
            Vec::new(),
        )
    };
    let cwd = plan.working_directory.as_deref();
    // Every protected root must resolve; one that cannot is a refusal, never a silent gap.
    let mut repositories = Vec::with_capacity(roots.repositories.len());
    for r in &roots.repositories {
        match repository(fs, r) {
            Ok(repo) => repositories.push(repo),
            Err(()) => return unresolved(&r.location),
        }
    }
    let Some(home_path) = fs.home() else {
        return unresolved("~");
    };
    let Ok(home) = locate(fs, &home_path.to_string_lossy()) else {
        return unresolved("~");
    };
    let app_data = match roots.app_data_root.as_deref().map(|a| (a, locate(fs, a))) {
        Some((a, Err(()))) => return unresolved(a),
        Some((_, Ok(p))) => Some(p),
        None => None,
    };
    let mut skills = Vec::with_capacity(roots.skill_roots.len());
    for root in &roots.skill_roots {
        match locate(fs, root) {
            Ok(p) => skills.push(p),
            Err(()) => return unresolved(root),
        }
    }
    let permitted = match &plan.scope {
        IntendedScope::PrimaryRepositoryDeletion(permit) => Some(permit.repository_id().clone()),
        _ => None,
    };
    for repo in &repositories {
        if Some(&repo.id) == permitted.as_ref() {
            continue;
        }
        protected_ids.extend(repo.root.located.identity);
        protected_ids.extend(repo.git_dirs.iter().filter_map(|g| g.located.identity));
    }
    protected_ids.extend(home.located.identity);
    protected_ids.extend(app_data.as_ref().and_then(|a| a.located.identity));
    protected_ids.extend(skills.iter().filter_map(|s| s.located.identity));
    let protected_dirs = || {
        repositories
            .iter()
            .flat_map(|r| std::iter::once(&r.root).chain(r.git_dirs.iter()))
            .chain(std::iter::once(&home))
            .chain(app_data.iter())
            .chain(skills.iter())
    };

    // Expand globs into a bounded explicit set; whole-content erasure of a protected directory
    // is refused even though the directory itself would survive.
    let mut expanded = Vec::new();
    for requested in &plan.targets {
        if !has_wildcard(requested) {
            expanded.push(requested.clone());
            continue;
        }
        let (parent, last) = match requested.rfind(['/', '\\']) {
            // Keep the separator of a root (`/`, `C:\`) so the parent still names the root.
            Some(i) if i == 0 || requested[..i].ends_with(':') => {
                (&requested[..=i], &requested[i + 1..])
            }
            Some(i) => (&requested[..i], &requested[i + 1..]),
            None => (".", requested.as_str()),
        };
        // Any glob directly inside a protected directory may match all of its entries
        // (`*`, `[!.]*`, `?*`, …): refused as whole-content erasure rather than pattern-guessed.
        if !has_wildcard(parent)
            && has_wildcard(last)
            && let Ok(dir) = fs.resolve(parent, cwd)
        {
            let dir = dir.target.unwrap_or(dir.entry);
            if dir.volume_root || protected_dirs().any(|p| same(&dir, &p.located)) {
                return decide(
                    PreflightVerdict::Blocked(if dir.volume_root {
                        BlockReason::FilesystemRoot
                    } else {
                        BlockReason::WholeContentErasure
                    }),
                    Some(requested),
                    Vec::new(),
                    Vec::new(),
                );
            }
        }
        match fs.expand(requested, cwd, MAX_TARGETS) {
            Ok(paths) => expanded.extend(paths),
            Err(ResolveError::TooMany { .. }) => {
                return decide(
                    PreflightVerdict::NeedsInput(InputNeeded::UnboundedExpansion),
                    Some(requested),
                    Vec::new(),
                    Vec::new(),
                );
            }
            Err(_) => {
                return decide(
                    PreflightVerdict::NeedsInput(InputNeeded::UnresolvedTarget),
                    Some(requested),
                    Vec::new(),
                    Vec::new(),
                );
            }
        }
        if expanded.len() > MAX_TARGETS {
            return decide(
                PreflightVerdict::NeedsInput(InputNeeded::UnboundedExpansion),
                Some(requested),
                Vec::new(),
                Vec::new(),
            );
        }
    }
    if plan.targets.is_empty() {
        return decide(
            PreflightVerdict::NeedsInput(InputNeeded::UnresolvedTarget),
            None,
            Vec::new(),
            Vec::new(),
        );
    }

    let mut checked = Vec::with_capacity(expanded.len());
    for requested in &expanded {
        let resolved = match fs.resolve(requested, cwd) {
            Ok(r) => r,
            Err(_) => {
                return decide(
                    PreflightVerdict::NeedsInput(InputNeeded::UnresolvedTarget),
                    Some(requested),
                    Vec::new(),
                    Vec::new(),
                );
            }
        };
        match classify(
            &resolved,
            plan,
            &repositories,
            Some(&home),
            (app_data.as_ref(), &skills),
            roots,
            fs,
        ) {
            Ok(class) => checked.push(CheckedTarget {
                requested: requested.clone(),
                target_identity: resolved.target.as_ref().and_then(|t| t.identity),
                entry: resolved.entry,
                entry_kind: resolved.entry_kind,
                parent_traversal: resolved.parent_traversal,
                class,
            }),
            Err(reason) => {
                return decide(
                    PreflightVerdict::Blocked(reason),
                    Some(requested),
                    Vec::new(),
                    Vec::new(),
                );
            }
        }
    }
    if plan.kind == DestructiveKind::Consequential {
        // Not refused, but its exact effect is not modeled: never allowed by default.
        return decide(
            PreflightVerdict::NeedsInput(InputNeeded::UnmodeledConsequentialCommand),
            plan.targets.first().map(String::as_str),
            Vec::new(),
            Vec::new(),
        );
    }
    decide(PreflightVerdict::Allowed, None, checked, protected_ids)
}

fn classify(
    r: &ResolvedPath,
    plan: &EffectPlan,
    repositories: &[Repository],
    home: Option<&Protected>,
    (app_data, skills): (Option<&Protected>, &[Protected]),
    roots: &ProtectedRoots,
    fs: &dyn FilesystemResolver,
) -> Result<TargetClass, BlockReason> {
    let kind = plan.kind;
    // The objects this step could destroy: the named entry, and what it resolves to when the
    // step follows links or recurses.
    let mut affected = vec![&r.entry];
    if (kind.follows() || kind.recursive())
        && let Some(t) = &r.target
    {
        affected.push(t);
    }
    if affected.iter().any(|a| a.volume_root) {
        return Err(BlockReason::FilesystemRoot);
    }
    let missing = r.entry_kind == EntryKind::Missing && r.target.is_none();
    let hits = |p: &Protected| {
        affected
            .iter()
            .any(|a| same(a, &p.located) || (kind.recursive() && ancestor_of(a, &p.located)))
    };
    // A skill folder, anything inside it, or a recursive step over it: a write there would be
    // observed as a skill change. Checked before application data, which holds a skill folder.
    if skills
        .iter()
        .any(|s| hits(s) || affected.iter().any(|x| ancestor_of(&s.located, x)))
    {
        return Err(BlockReason::SkillDirectory);
    }
    if let Some(a) = app_data
        && (hits(a) || affected.iter().any(|x| ancestor_of(&a.located, x)))
    {
        return Err(BlockReason::ApplicationData);
    }
    if let Some(h) = home
        && hits(h)
    {
        return Err(BlockReason::ProtectedUserDirectory);
    }
    let mut confirmed = false;
    for repo in repositories {
        let id = &repo.id;
        if hits(&repo.root) {
            if let IntendedScope::PrimaryRepositoryDeletion(permit) = &plan.scope {
                // The permit must come from the same store, restore epoch and registry revision
                // the roots were read from: a permit minted elsewhere matches nothing here.
                let same_store = roots.store.as_ref().is_some_and(|(store, epoch)| {
                    store == permit.store_id() && *epoch == permit.restore_epoch()
                }) && roots.revision == permit.registry_revision();
                let exact = same_store
                    && kind == DestructiveKind::RemoveTree
                    && r.entry_kind == EntryKind::Directory
                    && permit.repository_id() == id
                    && r.entry.identity.is_some()
                    && r.entry.identity == Some(permit.identity())
                    && same(&r.entry, &repo.root.located);
                if exact {
                    confirmed = true;
                    continue;
                }
                return Err(BlockReason::PermitMismatch);
            }
            return Err(BlockReason::PrimaryRepository {
                repository_id: id.clone(),
            });
        }
        // Git metadata: anything inside one of the repository's Git directories (matched by
        // identity), or a recursive step over a Git directory kept outside the root.
        for g in &repo.git_dirs {
            if affected
                .iter()
                .any(|a| within(a, &g.located) || (kind.recursive() && ancestor_of(a, &g.located)))
            {
                return Err(BlockReason::RepositoryMetadata {
                    repository_id: id.clone(),
                });
            }
        }
    }
    // A missing target destroys nothing, but only after the protected checks: creating or
    // writing a new entry inside a Git directory (a hook, a ref) is refused like changing one.
    if missing {
        return Ok(TargetClass::NoTarget);
    }
    match &plan.scope {
        IntendedScope::Ordinary => Ok(TargetClass::Ordinary),
        IntendedScope::PrimaryRepositoryDeletion(_) => {
            if confirmed {
                Ok(TargetClass::ConfirmedPrimaryRepository)
            } else {
                Err(BlockReason::PermitMismatch)
            }
        }
        IntendedScope::OwnedWorktreeCleanup { worktree_id } => {
            let Some(w) = roots.worktrees.iter().find(|w| &w.id == worktree_id) else {
                return Err(BlockReason::WorktreeNotOwned);
            };
            let Ok(tree) = locate(fs, &w.location) else {
                return Err(BlockReason::WorktreeNotOwned);
            };
            if repositories
                .iter()
                .any(|p| same(&p.root.located, &tree.located))
            {
                return Err(BlockReason::PrimaryRepository {
                    repository_id: w.repository_id.clone(),
                });
            }
            if !w.stopped || w.active_users > 0 {
                return Err(BlockReason::WorktreeInUse);
            }
            // The named entry (ancestors resolved, so a linked ancestor that leaves the worktree
            // is caught here) must be inside it, and so must whatever a following or recursive
            // step would reach through a final link.
            if !within(&r.entry, &tree.located) {
                return Err(BlockReason::EscapesScope);
            }
            if !affected.iter().all(|a| within(a, &tree.located)) {
                return Err(BlockReason::LinkEscape);
            }
            Ok(TargetClass::OwnedWorktree)
        }
    }
}

/// Re-resolves every target of an allowed decision immediately before the destructive step.
/// A changed registry, entry kind or identity refuses the step instead of enlarging it.
pub fn revalidate(
    fs: &dyn FilesystemResolver,
    decision: &PreflightDecision,
    current_registry_revision: &str,
) -> Result<(), ResolveError> {
    let Some(targets) = decision.allowed_targets() else {
        return Err(ResolveError::Changed(
            "the decision did not allow this step".into(),
        ));
    };
    if decision.registry_revision != current_registry_revision {
        return Err(ResolveError::Changed("protected roots changed".into()));
    }
    for t in targets {
        let now = fs.resolve(&t.entry.path.to_string_lossy(), None)?;
        let target_identity = now.target.as_ref().and_then(|x| x.identity);
        if now.entry_kind != t.entry_kind
            || now.entry.identity != t.entry.identity
            || target_identity != t.target_identity
        {
            return Err(ResolveError::Changed(format!(
                "{} changed since it was checked",
                t.requested
            )));
        }
    }
    Ok(())
}

/// What a destructive step actually did. Recursive deletion can be partial; the remaining
/// entries are reported instead of an all-or-nothing claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemovalOutcome {
    pub removed_entries: u64,
    pub remaining: Vec<String>,
    pub complete: bool,
}

/// Performs one checked step through verified, no-follow, descriptor-relative handles where the
/// platform provides them; refuses a case whose identity rule it cannot preserve.
pub trait DestructiveExecutor: Send + Sync {
    /// `protected` is [`PreflightDecision::protected_identities`]: a protected root or Git
    /// directory found inside the tree at execution time (renamed in after the preflight) is
    /// left in place and reported as remaining.
    fn execute(
        &self,
        target: &CheckedTarget,
        kind: DestructiveKind,
        protected: &[FileIdentity],
    ) -> Result<RemovalOutcome, ResolveError>;
}

/// Records a blocked or held decision in history, attributed to the invoking principal with its
/// work lineage and linked human authorization sources. Allowed decisions record nothing here;
/// the owning operation journals the effect.
pub fn record_refusal(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    operation: &str,
    decision: &PreflightDecision,
) -> Result<Option<i64>, StorageError> {
    let (kind, reason) = match &decision.verdict {
        PreflightVerdict::Allowed => return Ok(None),
        PreflightVerdict::Blocked(r) => ("DestructiveOperationBlocked", r.as_str()),
        PreflightVerdict::NeedsInput(n) => ("DestructiveOperationHeld", n.as_str()),
    };
    let repository = match &decision.verdict {
        PreflightVerdict::Blocked(
            BlockReason::PrimaryRepository { repository_id }
            | BlockReason::RepositoryMetadata { repository_id },
        ) => Some(repository_id.as_str()),
        _ => None,
    };
    let sources: Vec<&str> = ctx.authority_sources().iter().map(Id::as_str).collect();
    let aggregate_id = sha256_hex(
        format!(
            "{}/{}/{}",
            ctx.invocation_id().as_str(),
            operation,
            decision.offending.as_deref().unwrap_or("")
        )
        .as_bytes(),
    );
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history()
        .append(NewEvent {
            id,
            actor: ctx.principal().clone(),
            project_id: ctx.project_id().cloned(),
            root_id: ctx.root_id().cloned(),
            operation_id: None,
            aggregate_kind: "destructive_preflight".into(),
            aggregate_id,
            aggregate_revision: 1,
            kind: kind.into(),
            payload: json!({
                "operation": operation,
                "effect": format!("{:?}", decision.kind),
                "reason": reason,
                "rationale": decision.rationale(),
                "target": decision.offending,
                "repositoryId": repository,
                "principalKind": ctx.principal_kind().as_str().to_string(),
                "authoritySources": sources,
                "registryRevision": decision.registry_revision,
            }),
        })
        .map(Some)
}
