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
use crate::ports::storage::{NewEvent, RegisteredRepository, StorageError, WriteTransaction};
use crate::protocol::{EventId, Id};
use serde_json::json;
use std::path::{Path, PathBuf};

/// Most explicit targets one plan may expand to; more is `NeedsInput`, never a partial check.
pub const MAX_TARGETS: usize = 256;

/// Stable identity of a filesystem object (device/volume and file number).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
}
impl ProtectedRoots {
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
        }
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
}
impl DestructiveKind {
    fn recursive(self) -> bool {
        matches!(self, Self::RemoveTree | Self::EraseContents)
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
}
impl InputNeeded {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnboundedExpansion => "UnboundedExpansion",
            Self::UnresolvedTarget => "UnresolvedTarget",
            Self::UnresolvedDeclaredTarget => "UnresolvedDeclaredTarget",
            Self::UnresolvedExpansion => "UnresolvedExpansion",
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
}
impl PreflightDecision {
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
fn locate(fs: &dyn FilesystemResolver, location: &str) -> Option<Protected> {
    let r = fs.resolve(location, None).ok()?;
    Some(Protected {
        located: r.target.unwrap_or(r.entry),
    })
}

fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}
fn path_within(inner: &Path, outer: &Path) -> bool {
    if cfg!(windows) {
        let (i, o) = (
            PathBuf::from(inner.to_string_lossy().to_lowercase()),
            PathBuf::from(outer.to_string_lossy().to_lowercase()),
        );
        i.starts_with(o)
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
    s.contains(['*', '?', '['])
}

/// Runs the preflight. Refusals are total: one blocked or unresolved target refuses the plan.
pub fn preflight(
    fs: &dyn FilesystemResolver,
    roots: &ProtectedRoots,
    plan: &EffectPlan,
) -> PreflightDecision {
    let decide = |verdict, offending: Option<&str>, targets| PreflightDecision {
        verdict,
        kind: plan.kind,
        offending: offending.map(str::to_owned),
        targets,
        registry_revision: roots.revision.clone(),
    };
    let cwd = plan.working_directory.as_deref();
    let repositories: Vec<(Id, Protected)> = roots
        .repositories
        .iter()
        .filter_map(|r| locate(fs, &r.location).map(|p| (r.id.clone(), p)))
        .collect();
    let home = fs.home().and_then(|h| locate(fs, &h.to_string_lossy()));
    let app_data = roots.app_data_root.as_deref().and_then(|a| locate(fs, a));
    let protected_dirs = || {
        repositories
            .iter()
            .map(|(_, p)| p)
            .chain(home.iter())
            .chain(app_data.iter())
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
        if !has_wildcard(parent)
            && !last.is_empty()
            && last.chars().all(|c| c == '*' || c == '.')
            && let Ok(dir) = fs.resolve(parent, cwd)
        {
            let dir = dir.target.unwrap_or(dir.entry);
            let worktree_cleanup = matches!(plan.scope, IntendedScope::OwnedWorktreeCleanup { .. });
            if dir.volume_root
                || (!worktree_cleanup && protected_dirs().any(|p| same(&dir, &p.located)))
            {
                return decide(
                    PreflightVerdict::Blocked(if dir.volume_root {
                        BlockReason::FilesystemRoot
                    } else {
                        BlockReason::WholeContentErasure
                    }),
                    Some(requested),
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
                );
            }
            Err(_) => {
                return decide(
                    PreflightVerdict::NeedsInput(InputNeeded::UnresolvedTarget),
                    Some(requested),
                    Vec::new(),
                );
            }
        }
        if expanded.len() > MAX_TARGETS {
            return decide(
                PreflightVerdict::NeedsInput(InputNeeded::UnboundedExpansion),
                Some(requested),
                Vec::new(),
            );
        }
    }
    if plan.targets.is_empty() {
        return decide(
            PreflightVerdict::NeedsInput(InputNeeded::UnresolvedTarget),
            None,
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
                );
            }
        };
        match classify(
            &resolved,
            plan,
            &repositories,
            home.as_ref(),
            app_data.as_ref(),
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
                );
            }
        }
    }
    decide(PreflightVerdict::Allowed, None, checked)
}

fn classify(
    r: &ResolvedPath,
    plan: &EffectPlan,
    repositories: &[(Id, Protected)],
    home: Option<&Protected>,
    app_data: Option<&Protected>,
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
    if r.entry_kind == EntryKind::Missing && r.target.is_none() {
        return Ok(TargetClass::NoTarget);
    }
    let hits = |p: &Protected| {
        affected
            .iter()
            .any(|a| same(a, &p.located) || (kind.recursive() && ancestor_of(a, &p.located)))
    };
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
    for (id, repo) in repositories {
        if hits(repo) {
            if let IntendedScope::PrimaryRepositoryDeletion(permit) = &plan.scope {
                let exact = kind == DestructiveKind::RemoveTree
                    && r.entry_kind == EntryKind::Directory
                    && permit.repository_id() == id
                    && r.entry.identity.is_some()
                    && r.entry.identity == Some(permit.identity())
                    && same(&r.entry, &repo.located);
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
        // The repository's `.git` metadata: its parent is the registered root.
        for a in &affected {
            let is_metadata = a
                .path
                .file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case(".git"))
                && match (a.ancestors.first(), repo.located.identity) {
                    (Some(parent), Some(root)) => *parent == root,
                    _ => a
                        .path
                        .parent()
                        .is_some_and(|p| same_path(p, &repo.located.path)),
                };
            if is_metadata {
                return Err(BlockReason::RepositoryMetadata {
                    repository_id: id.clone(),
                });
            }
        }
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
            let Some(tree) = locate(fs, &w.location) else {
                return Err(BlockReason::WorktreeNotOwned);
            };
            if repositories
                .iter()
                .any(|(_, p)| same(&p.located, &tree.located))
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
    fn execute(
        &self,
        target: &CheckedTarget,
        kind: DestructiveKind,
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
                "principalKind": format!("{:?}", ctx.principal_kind()),
                "authoritySources": sources,
                "registryRevision": decision.registry_revision,
            }),
        })
        .map(Some)
}
