//! Dedicated primary repository deletion (SEC-04): the only path past the primary-root guard.
//!
//! Anyone, including an agent, may [`prepare`] a concrete preview. Only the user confirms it
//! ([`confirm`]), which mints a single-use, short-lived `PrimaryRepositoryDeletion` grant bound to
//! the preview digest (resolved identity, registry revision, references, worktrees). Before the
//! typed deletion, [`authorize`] re-checks a freshly prepared preview against that binding and
//! spends the grant in the same transaction, returning the [`PrimaryDeletionPermit`] the
//! destructive preflight requires. An agent is refused at every step that confers authority,
//! whatever its profile, role or approvals; an ordinary operation approval is a different grant
//! kind and never satisfies the confirmation. Filesystem roots stay blocked even with a permit,
//! network remotes are never deleted and no automatic backup or undo is implied.
//!
//! A permit is bound to the store, restore epoch and protected-root registry revision it was
//! spent on, and the preflight honors it only against roots read from that same store
//! ([`super::destructive::ProtectedRoots::read`]); a permit minted on any other store (one a
//! caller opened or fabricated) matches nothing. The store trait is not sealed: its production
//! implementation lives in another crate, so a seal would only be a convention, and anyone in
//! the process who can open stores can also touch the filesystem directly. The binding closes
//! the confusion on the supported path, which is what the guard promises.
//!
//! The confirming steps take the store and run their own unit of work: they are a dedicated
//! user-ingress path, not something an operation handler (which only ever holds the gateway's
//! handler view of a transaction) can reach.
use super::destructive::{
    self, DestructiveExecutor, DestructiveKind, EffectPlan, FileIdentity, FilesystemResolver,
    IntendedScope, OwnedWorktree, PreflightVerdict, ProtectedRoots, RemovalOutcome,
};
use super::service::{self, AdminError, GrantBounds};
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::GrantUse;
use crate::ports::storage::{
    ApplicationStore, AuthorityGrant, EventFilter, GrantKind, ReadOptions, StorageError,
    WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::protocol::{ActorId, Id, ProjectId, StoreId};
use serde_json::{Value, json};

/// History aggregate of a repository deletion (`aggregate_id` = repository ID).
pub const DELETION_AGGREGATE: &str = "repository_deletion";
/// Longest lifetime of a deletion confirmation: it is a short, single-use act, not a standing
/// permission (engineering default).
pub const MAX_CONFIRMATION_LIFETIME_MS: i64 = 15 * 60 * 1000;
/// What the dedicated action never removes.
pub const RETAINED: [&str; 3] = [
    "network remotes",
    "work history and outcomes",
    "artifacts under their own retention",
];

/// Proof that the user's dedicated confirmation was spent for exactly this repository. Only
/// [`authorize`] constructs one; it is not `Clone`, so one confirmation covers one step.
#[derive(Debug)]
pub struct PrimaryDeletionPermit {
    repository_id: Id,
    identity: FileIdentity,
    grant_id: Id,
    actor: ActorId,
    store_id: StoreId,
    restore_epoch: i64,
    registry_revision: String,
}
impl PrimaryDeletionPermit {
    /// The store the confirmation was spent on.
    pub fn store_id(&self) -> &StoreId {
        &self.store_id
    }
    pub fn restore_epoch(&self) -> i64 {
        self.restore_epoch
    }
    /// The protected-root registry revision the confirmed preview was prepared against.
    pub fn registry_revision(&self) -> &str {
        &self.registry_revision
    }
    pub fn repository_id(&self) -> &Id {
        &self.repository_id
    }
    pub fn identity(&self) -> FileIdentity {
        self.identity
    }
    pub fn grant_id(&self) -> &Id {
        &self.grant_id
    }
    pub fn actor(&self) -> &ActorId {
        &self.actor
    }
}

/// Live dependencies the repository's owner reports for the preview.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepositoryDependencies {
    pub project_references: Vec<ProjectId>,
    pub external_users: u32,
    pub active_work: Vec<Id>,
}

/// The concrete preview a confirmation binds to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletionPreview {
    pub repository_id: Id,
    pub location: String,
    pub resolved: String,
    pub identity: FileIdentity,
    pub registry_revision: String,
    pub dependencies: RepositoryDependencies,
    pub worktrees: Vec<OwnedWorktree>,
    pub retained: [&'static str; 3],
    /// Conditions that block confirmation until the user resolves them separately.
    pub blockers: Vec<&'static str>,
    pub digest: String,
}

#[derive(Clone, Debug)]
pub enum DeletionRefusal {
    NotRegistered,
    /// This platform's filesystem adapter performs no destructive steps.
    Unsupported,
    /// The registered location cannot be resolved to a stable directory identity.
    Unresolvable,
    FilesystemRoot,
    Blocked(Vec<&'static str>),
    /// Only the user, through the dedicated confirmation surface.
    HumanOnly,
    /// No current, unspent confirmation of this kind for this user.
    ConfirmationUnavailable,
    /// The target, its references or the registry changed since confirmation.
    Changed,
    /// The destructive-operation guard refused the removal step (its rationale).
    Guarded(&'static str),
    /// The removal started and failed; the partial outcome is recorded.
    Failed(String),
    /// The removal ran (this is what it did) but recording its outcome failed.
    Unrecorded(RemovalOutcome),
    Storage(StorageError),
}
impl From<StorageError> for DeletionRefusal {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}
impl From<AdminError> for DeletionRefusal {
    fn from(e: AdminError) -> Self {
        match e {
            AdminError::Storage(e) => Self::Storage(e),
            AdminError::Denied(_) => Self::HumanOnly,
            AdminError::Invalid(_) => Self::ConfirmationUnavailable,
        }
    }
}

/// Builds the concrete preview. Available to agents and the user alike; it confers nothing.
pub fn prepare(
    fs: &dyn FilesystemResolver,
    roots: &ProtectedRoots,
    repository_id: &Id,
    dependencies: RepositoryDependencies,
) -> Result<DeletionPreview, DeletionRefusal> {
    // Refused before anything is confirmed where the folder could not actually be removed.
    if !fs.destructive_steps_supported() {
        return Err(DeletionRefusal::Unsupported);
    }
    let repo = roots
        .repositories
        .iter()
        .find(|r| &r.id == repository_id)
        .ok_or(DeletionRefusal::NotRegistered)?;
    let resolved = fs
        .resolve(&repo.location, None)
        .map_err(|_| DeletionRefusal::Unresolvable)?;
    let target = resolved.target.ok_or(DeletionRefusal::Unresolvable)?;
    if target.volume_root || resolved.entry.volume_root {
        return Err(DeletionRefusal::FilesystemRoot);
    }
    let identity = match (target.is_directory, target.identity) {
        (true, Some(identity)) => identity,
        _ => return Err(DeletionRefusal::Unresolvable),
    };
    let worktrees: Vec<OwnedWorktree> = roots
        .worktrees
        .iter()
        .filter(|w| &w.repository_id == repository_id)
        .cloned()
        .collect();
    let mut blockers = Vec::new();
    if dependencies.project_references.len() > 1 {
        blockers.push("another project still references this repository");
    }
    if dependencies.external_users > 0 {
        blockers.push("the repository has unresolved external users");
    }
    if !dependencies.active_work.is_empty()
        || worktrees.iter().any(|w| !w.stopped || w.active_users > 0)
    {
        blockers.push("work is still active in this repository");
    }
    let basis = json!({
        "repositoryId": repository_id.as_str(),
        "location": repo.location,
        "resolved": target.path.to_string_lossy(),
        "identity": [identity.volume.to_string(), identity.file.to_string()],
        "registryRevision": roots.revision,
        "projects": dependencies.project_references.iter().map(|p| p.0.as_str()).collect::<Vec<_>>(),
        "externalUsers": dependencies.external_users,
        "activeWork": dependencies.active_work.iter().map(Id::as_str).collect::<Vec<_>>(),
        "worktrees": worktrees.iter().map(|w| json!([w.id.as_str(), w.location, w.stopped, w.active_users])).collect::<Vec<_>>(),
    });
    let digest = crate::fingerprint::canonical_digest(&basis).expect("bounded preview");
    Ok(DeletionPreview {
        repository_id: repository_id.clone(),
        location: repo.location.clone(),
        resolved: target.path.to_string_lossy().into_owned(),
        identity,
        registry_revision: roots.revision.clone(),
        dependencies,
        worktrees,
        retained: RETAINED,
        blockers,
        digest,
    })
}

/// Most work roots of one project read per page when looking for live work.
const ROOTS_PAGE: u16 = 500;

/// The repository's live dependencies as the store records them: the projects it is a member of
/// and the live activations of those projects' work (any live work there may use its folder).
pub fn dependencies(
    store: &dyn ApplicationStore,
    repository: &Id,
) -> Result<RepositoryDependencies, StorageError> {
    let repository = repository.clone();
    read_snapshot(store, &ReadOptions::default(), move |s| {
        let projects = s.projects().repository_projects(&repository)?;
        let mut active_work = Vec::new();
        for project in &projects {
            let mut after = None;
            loop {
                let page = s
                    .runs()
                    .project_roots(project, after.as_ref(), ROOTS_PAGE)?;
                for root in &page {
                    if let Some(a) = s.runs().live_activation(root)? {
                        active_work.push(a.id.clone());
                    }
                }
                if page.len() < usize::from(ROOTS_PAGE) {
                    break;
                }
                after = page.last().cloned();
            }
        }
        Ok(RepositoryDependencies {
            project_references: projects,
            external_users: 0,
            active_work,
        })
    })
}

/// Most recorded task worktrees read per page.
const WORKTREES_PAGE: u16 = 500;

/// The repository's recorded task worktrees that are not removed, and their current use: a
/// worktree is in use while a Git write holds it, and stopped unless its owning work has a live
/// activation. Deleting the primary checkout removes the Git metadata they depend on.
pub fn worktrees(
    store: &dyn ApplicationStore,
    repository: &Id,
) -> Result<Vec<OwnedWorktree>, StorageError> {
    let repository = repository.clone();
    read_snapshot(store, &ReadOptions::default(), move |s| {
        let mut out = Vec::new();
        let mut after: Option<Id> = None;
        loop {
            let page =
                s.git()
                    .git_repository_worktrees(&repository, after.as_ref(), WORKTREES_PAGE)?;
            for w in page.iter().filter(|w| !w.removed) {
                let live = match &w.owner_root_id {
                    Some(root) => s.runs().live_activation(root)?.is_some(),
                    None => false,
                };
                out.push(OwnedWorktree {
                    id: w.id.clone(),
                    repository_id: w.repository_id.clone(),
                    location: w.path.clone(),
                    stopped: !live,
                    active_users: u32::from(w.pending_operation_id.is_some()),
                });
            }
            if page.len() < usize::from(WORKTREES_PAGE) {
                return Ok(out);
            }
            after = page.last().map(|w| w.id.clone());
        }
    })
}

/// The protected roots as they are now, with `repository`'s recorded task worktrees.
fn current_roots(
    store: &dyn ApplicationStore,
    app_data_root: Option<String>,
    repository: &Id,
) -> Result<ProtectedRoots, StorageError> {
    ProtectedRoots::read(store, worktrees(store, repository)?, app_data_root)
}

/// The preview of `repository` as it is now: the protected roots, its recorded task worktrees
/// and its dependencies read from the store. Like [`prepare`], it confers nothing.
pub fn preview(
    store: &dyn ApplicationStore,
    fs: &dyn FilesystemResolver,
    app_data_root: Option<String>,
    repository: &Id,
) -> Result<DeletionPreview, DeletionRefusal> {
    let roots = current_roots(store, app_data_root, repository)?;
    prepare(fs, &roots, repository, dependencies(store, repository)?)
}

/// The dedicated primary repository deletion of the user's trusted surface, after the user
/// reviewed `reviewed`: re-prepares the repository as it is now, confirms exactly the reviewed
/// preview and spends that confirmation only if the current one matches it ([`authorize`]), then
/// removes the folder through the destructive-operation guard and records the outcome. The
/// registration, work history, remotes and artifacts are kept ([`RETAINED`]). An agent is
/// refused before anything is confirmed.
pub fn delete<F: FilesystemResolver + DestructiveExecutor>(
    store: &dyn ApplicationStore,
    fs: &F,
    app_data_root: Option<String>,
    ctx: &InvocationContext,
    reviewed: &DeletionPreview,
) -> Result<RemovalOutcome, DeletionRefusal> {
    let current = preview(store, fs, app_data_root.clone(), &reviewed.repository_id)?;
    let grant = confirm_within(store, ctx, reviewed, MAX_CONFIRMATION_LIFETIME_MS)?;
    let permit = authorize(store, ctx, &grant.id, &current)?;
    let repository = &current.repository_id;
    let registry = current_roots(store, app_data_root.clone(), repository)?;
    let plan = EffectPlan {
        kind: DestructiveKind::RemoveTree,
        targets: vec![current.location.clone()],
        working_directory: None,
        scope: IntendedScope::PrimaryRepositoryDeletion(permit),
        // The registered location names itself: it is never expanded as a pattern.
        literal: true,
    };
    let decision = destructive::preflight(fs, &registry, &plan);
    let IntendedScope::PrimaryRepositoryDeletion(permit) = plan.scope else {
        unreachable!("the plan keeps its scope")
    };
    let target = match (&decision.verdict, decision.allowed_targets()) {
        (PreflightVerdict::Allowed, Some([target])) => target,
        _ => return Err(DeletionRefusal::Guarded(decision.rationale())),
    };
    // Everything the preview binds is read again immediately before the step: a repository
    // registered, a worktree recorded or a project membership or live work added since the
    // preflight changes it and refuses the step.
    let now = preview(store, fs, app_data_root, repository)?;
    if now.digest != current.digest {
        return Err(DeletionRefusal::Changed);
    }
    let step = destructive::revalidate(fs, &decision, &now.registry_revision)
        .map_err(|_| DeletionRefusal::Changed)
        .and_then(|()| {
            fs.execute(
                target,
                DestructiveKind::RemoveTree,
                decision.protected_identities(),
            )
            .map_err(|e| DeletionRefusal::Failed(format!("{e:?}")))
        });
    let outcome = match step {
        Ok(outcome) => outcome,
        Err(DeletionRefusal::Failed(why)) => {
            let partial = RemovalOutcome {
                removed_entries: 0,
                remaining: vec![current.location.clone()],
                complete: false,
            };
            return Err(match record_outcome(store, ctx, &permit, &partial) {
                Ok(_) => DeletionRefusal::Failed(why),
                Err(_) => DeletionRefusal::Unrecorded(partial),
            });
        }
        Err(other) => return Err(other),
    };
    // Once the folder was removed, a failure to record it never reads as nothing removed.
    match record_outcome(store, ctx, &permit, &outcome) {
        Ok(_) => Ok(outcome),
        Err(_) => Err(DeletionRefusal::Unrecorded(outcome)),
    }
}

fn require_user(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
) -> Result<(), DeletionRefusal> {
    if ctx.principal_kind() != PrincipalKind::Human
        || tx.actors().actor_kind(ctx.principal())? != Some(PrincipalKind::Human)
    {
        return Err(DeletionRefusal::HumanOnly);
    }
    if &tx.store().store_id != ctx.store_id() {
        return Err(DeletionRefusal::HumanOnly);
    }
    Ok(())
}

/// The next revision of this repository's deletion history (real, not assumed).
fn next_revision(tx: &mut dyn WriteTransaction, repository: &Id) -> Result<i64, StorageError> {
    let filter = EventFilter::Aggregate {
        kind: DELETION_AGGREGATE.into(),
        id: repository.as_str().to_owned(),
    };
    let mut after = 0;
    let mut last = 0;
    loop {
        let page = tx.history().page(&filter, after, 500)?;
        last = page
            .events
            .iter()
            .map(|e| e.aggregate_revision)
            .fold(last, i64::max);
        if !page.has_more {
            return Ok(last + 1);
        }
        after = page.next_after;
    }
}

/// Runs one dedicated deletion step in its own unit of work on the store. These steps take the
/// store, never a use case's transaction, so no operation handler can reach them.
fn dedicated<T: Send + 'static>(
    store: &dyn ApplicationStore,
    work: impl FnOnce(&mut dyn WriteTransaction) -> Result<T, DeletionRefusal> + Send + 'static,
) -> Result<T, DeletionRefusal> {
    run_write(store, WriteOptions::default(), move |tx| Ok(work(tx)))
        .map_err(DeletionRefusal::Storage)?
}

/// The user's dedicated confirmation from the trusted local surface: mints a single-use grant
/// bound to this exact preview, held by the user, expiring within
/// [`MAX_CONFIRMATION_LIFETIME_MS`].
pub fn confirm(
    store: &dyn ApplicationStore,
    ctx: &InvocationContext,
    preview: &DeletionPreview,
    expires_at_ms: i64,
) -> Result<AuthorityGrant, DeletionRefusal> {
    confirm_at(store, ctx, preview, move |_| expires_at_ms)
}

/// [`confirm`] expiring `lifetime_ms` after the store's current time.
fn confirm_within(
    store: &dyn ApplicationStore,
    ctx: &InvocationContext,
    preview: &DeletionPreview,
    lifetime_ms: i64,
) -> Result<AuthorityGrant, DeletionRefusal> {
    confirm_at(store, ctx, preview, move |now| {
        now.saturating_add(lifetime_ms)
    })
}

fn confirm_at(
    store: &dyn ApplicationStore,
    ctx: &InvocationContext,
    preview: &DeletionPreview,
    expiry: impl FnOnce(i64) -> i64 + Send + 'static,
) -> Result<AuthorityGrant, DeletionRefusal> {
    let (ctx, preview) = (ctx.clone(), preview.clone());
    dedicated(store, move |tx| {
        require_user(tx, &ctx)?;
        let expires_at_ms = expiry(tx.now_ms());
        if !preview.blockers.is_empty() {
            return Err(DeletionRefusal::Blocked(preview.blockers.clone()));
        }
        let binding = json!({
            "repositoryId": preview.repository_id.as_str(),
            "previewDigest": preview.digest,
        });
        let grant = service::issue(
            tx,
            &ctx,
            ctx.principal().clone(),
            GrantKind::PrimaryRepositoryDeletion,
            binding,
            GrantBounds {
                expires_at_ms,
                uses: 1,
            },
            MAX_CONFIRMATION_LIFETIME_MS,
        )?;
        let revision = next_revision(tx, &preview.repository_id)?;
        service::record(
            tx,
            &ctx,
            DELETION_AGGREGATE,
            preview.repository_id.as_str(),
            revision,
            "RepositoryDeletionConfirmed",
            json!({"grantId": grant.id.as_str(), "previewDigest": preview.digest, "resolved": preview.resolved}),
        )?;
        Ok(grant)
    })
}

/// Re-checks a freshly prepared preview against the confirmation and spends it, in its own
/// unit of work, immediately before the typed deletion. Agents are always refused.
pub fn authorize(
    store: &dyn ApplicationStore,
    ctx: &InvocationContext,
    grant_id: &Id,
    current: &DeletionPreview,
) -> Result<PrimaryDeletionPermit, DeletionRefusal> {
    let (ctx, grant_id, current) = (ctx.clone(), grant_id.clone(), current.clone());
    dedicated(store, move |tx| {
        require_user(tx, &ctx)?;
        let grant = tx
            .authority()
            .authority_grant(&grant_id)?
            .ok_or(DeletionRefusal::ConfirmationUnavailable)?;
        let now = tx.now_ms();
        if grant.kind != GrantKind::PrimaryRepositoryDeletion
            || &grant.issued_by != ctx.principal()
            || !grant.usable_by(ctx.principal(), now)
        {
            return Err(DeletionRefusal::ConfirmationUnavailable);
        }
        let bound = |k: &str| grant.binding.get(k).and_then(Value::as_str);
        if bound("repositoryId") != Some(current.repository_id.as_str())
            || bound("previewDigest") != Some(current.digest.as_str())
        {
            return Err(DeletionRefusal::Changed);
        }
        if !current.blockers.is_empty() {
            return Err(DeletionRefusal::Blocked(current.blockers.clone()));
        }
        tx.authority_writes(&super::CAPABILITY, ctx.principal())?
            .consume_grant(
                &GrantUse {
                    grant_id: grant.id.clone(),
                    expected_revision: grant.revision,
                },
                ctx.principal(),
            )
            .map_err(|e| match e {
                StorageError::ScopeDenied | StorageError::RevisionConflict(_) => {
                    DeletionRefusal::ConfirmationUnavailable
                }
                other => DeletionRefusal::Storage(other),
            })?;
        let revision = next_revision(tx, &current.repository_id)?;
        service::record(
            tx,
            &ctx,
            DELETION_AGGREGATE,
            current.repository_id.as_str(),
            revision,
            "RepositoryDeletionAuthorized",
            json!({"grantId": grant.id.as_str(), "previewDigest": current.digest}),
        )?;
        let store = tx.store();
        Ok(PrimaryDeletionPermit {
            repository_id: current.repository_id.clone(),
            identity: current.identity,
            grant_id: grant.id,
            actor: ctx.principal().clone(),
            store_id: store.store_id.clone(),
            restore_epoch: store.restore_epoch,
            registry_revision: current.registry_revision.clone(),
        })
    })
}

/// Records the actual outcome, partial or complete. Work history and remotes are untouched.
pub fn record_outcome(
    store: &dyn ApplicationStore,
    ctx: &InvocationContext,
    permit: &PrimaryDeletionPermit,
    outcome: &RemovalOutcome,
) -> Result<i64, DeletionRefusal> {
    let (ctx, repository, grant, outcome) = (
        ctx.clone(),
        permit.repository_id.clone(),
        permit.grant_id.clone(),
        outcome.clone(),
    );
    dedicated(store, move |tx| {
        require_user(tx, &ctx)?;
        let revision = next_revision(tx, &repository)?;
        Ok(service::record(
            tx,
            &ctx,
            DELETION_AGGREGATE,
            repository.as_str(),
            revision,
            if outcome.complete {
                "RepositoryDeletionCompleted"
            } else {
                "RepositoryDeletionPartial"
            },
            json!({
                "grantId": grant.as_str(),
                "removedEntries": outcome.removed_entries,
                "remaining": outcome.remaining,
                "retained": RETAINED,
            }),
        )?)
    })
}
