//! Journaled writes of the Git owner. A worktree reservation serializes writes; an unknown
//! result keeps the reservation until observation proves the requested postcondition.
mod attach;
mod auto_cleanup;
mod documentation;
pub(super) use auto_cleanup::CleanupSchedule;
mod delivery;
mod feature_delivery;
pub(crate) use feature_delivery::{
    feature_head_digest, requires_local_integration, waits_for_publication,
};
mod pull_request;
mod push;
use super::*;
use crate::authority::destructive::EntryKind;
use crate::context::PrincipalKind;
use crate::gateway::AdmittedCommand;
use crate::locks::RwLockExt;
use crate::ports::storage::*;
use crate::ports::{AuthorityFence, CommandHandler, Decided, HandlerFailure};
use crate::protocol::data::GitCommand;
use crate::protocol::records::{GitActionKind, OperationRef};
use crate::protocol::results::DomainDecision;
use crate::protocol::{ActorId, AssignmentId, EventId, OperationId, ProjectId, RootId};
use crate::services::effect::{EffectOutcome, JournaledEffect};
use crate::services::kernel::{counter, decided};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Read;
use std::path::PathBuf;

pub const EFFECT_CONSUMER: &str = "git_effects";
pub(crate) const DOCUMENTATION_PATCH_ARGS: [&str; 4] =
    ["apply", "--index", "--whitespace=nowarn", "-"];

/// Host-registered root for newly created worktrees. A tool selects only the opaque ID.
#[derive(Clone)]
pub struct RegisteredGitRoot {
    pub id: Id,
    pub path: PathBuf,
}
fn initial_policy_revision() -> i64 {
    1
}

#[derive(Clone, Serialize, Deserialize)]
struct Job {
    actor: ActorId,
    kind: String,
    session: Option<Id>,
    project: ProjectId,
    root: Option<RootId>,
    assignment: Option<AssignmentId>,
    restore_epoch: i64,
    #[serde(default = "initial_policy_revision")]
    policy_revision: i64,
    repository_revision: i64,
    work_revision: i64,
    root_epochs: Vec<(RootId, i64)>,
    assignment_generation: Option<i64>,
    delivery_id: Id,
    feature_id: Option<Id>,
    fences: Vec<AuthorityFence>,
    tree: StoredGitWorktree,
    action: Action,
    #[serde(default)]
    reviewed_binding: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cleanup_proof: Option<delivery::CleanupProof>,
}
#[derive(Clone, Serialize, Deserialize)]
enum Action {
    Integrate,
    PullRequest {
        repository: super::github::Repository,
        head: String,
        base: String,
        title: String,
        description: String,
        expected: GitOid,
        covered: Vec<(Id, i64)>,
        #[serde(default)]
        final_feature: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        integrated_heads: Vec<GitOid>,
    },
    Create,
    Inspect,
    Documentation(Box<crate::services::graph::repository_analysis::DocumentationDraft>),
    Cleanup,
    Commit {
        message: String,
        expected: GitOid,
    },
    Push {
        #[serde(default)]
        transport_digest: Option<String>,
        remote: String,
        url: String,
        target: String,
        expected: GitOid,
        remote_head: Option<GitOid>,
        force: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_publication: Option<Box<feature_delivery::TaskPublication>>,
    },
}
struct StagedMessage(Result<String, DomainErrorCode>, Option<Value>);

fn reviewed_commit_matches(git: &Git<'_>, path: &Path, job: &Job, head: &str) -> bool {
    let (Action::Commit { message, expected }, Some(binding)) =
        (&job.action, &job.reviewed_binding)
    else {
        return false;
    };
    let message = if message.ends_with('\n') {
        message.clone()
    } else {
        format!("{message}\n")
    };
    git.commit_message(path, head)
        .is_ok_and(|actual| actual == message)
        && git
            .committed_commit_manifest(path, &expected.hex, head)
            .is_ok_and(|manifest| {
                super::approval::compact_commit_manifest(&manifest)
                    .ok()
                    .as_ref()
                    == Some(&binding["commitManifest"])
            })
}

impl Job {
    /// The invocation context for this job, or `None` when the stored principal kind is not
    /// recognised (the effect is then refused rather than run as another kind).
    fn context(&self, service: &GitService, operation: &OperationId) -> Option<InvocationContext> {
        let kind = PrincipalKind::parse(&self.kind)?;
        let mut ctx = InvocationContext::new(
            self.actor.clone(),
            kind,
            operation.0.clone(),
            Id::new("git-effects").expect("namespace"),
            service.store.info().store_id,
            counter(self.restore_epoch),
            rev(self.policy_revision),
        )
        .with_lineage(
            Some(self.project.clone()),
            self.root.clone(),
            self.assignment.clone(),
        );
        if let Some(session) = &self.session {
            ctx = ctx.with_session(session.clone());
        }
        Some(ctx)
    }
}

fn storage_failure(error: StorageError) -> EffectOutcome {
    // Storage diagnostics never include helper stderr, credentials, commit messages or URLs.
    let text = format!("Git state could not be verified: {error}");
    EffectOutcome::Unknown(text)
}
fn failed(code: DomainErrorCode, reason: &str) -> EffectOutcome {
    EffectOutcome::Failed(json!({"code":code,"kind":"git","reason":reason}).to_string())
}
fn denied(kind: &str) -> EffectOutcome {
    failed(DomainErrorCode::PermissionDenied, kind)
}
fn changed(kind: &str) -> EffectOutcome {
    failed(DomainErrorCode::SourceChanged, kind)
}
fn oid(head: String) -> GitOid {
    GitOid {
        format: if head.len() == 64 {
            ObjectFormat::Sha256
        } else {
            ObjectFormat::Sha1
        },
        hex: head,
    }
}

fn cleanup_stage(path: &Path, operation: &OperationId) -> Option<PathBuf> {
    let digest = crate::fingerprint::sha256_hex(operation.0.as_str().as_bytes());
    Some(
        path.parent()?
            .join(format!(".devworks-cleanup-{}", &digest[..24])),
    )
}

fn definite_git_refusal(outcome: &Result<Vec<u8>, GitFault>) -> bool {
    matches!(
        outcome,
        Err(GitFault::Helper(
            HelperFault::Refused(_) | HelperFault::Spawn(_) | HelperFault::Failed { .. }
        ))
    )
}

fn cleanup_identity_matches(git: &Git<'_>, expected: &StoredGitWorktree, path: &Path) -> bool {
    expected
        .location
        .as_ref()
        .zip(path.to_str())
        .is_some_and(|(location, path)| {
            super::location::require_worktree_location(git, path, location).is_ok()
        })
}

fn require_clean_checkout(
    git: &Git<'_>,
    expected: &StoredGitWorktree,
    path: &Path,
) -> Result<(), &'static str> {
    if !cleanup_identity_matches(git, expected, path) {
        return Err("could not verify the physical worktree and Git metadata");
    }
    let observed = git
        .revision_observation(path)
        .map_err(|_| "could not verify the worktree revision and status")?;
    if observed.head != expected.head.hex {
        return Err("the worktree head changed");
    }
    if observed.branch.as_deref() != Some(expected.branch.as_str()) {
        return Err("the worktree is no longer on its registered branch");
    }
    if observed.dirty {
        return Err("the worktree has tracked or untracked changes");
    }
    if git
        .has_status_hidden_entries(path)
        .map_err(|_| "could not verify the worktree index flags")?
    {
        return Err("the worktree index has entries that Git status can hide");
    }
    if git
        .has_ignored_files(path)
        .map_err(|_| "could not verify whether the worktree has ignored files")?
    {
        return Err("the worktree has ignored local files");
    }
    Ok(())
}

impl GitService {
    pub(super) fn root_path(&self, id: &Id) -> Result<PathBuf, Failure> {
        let root = self
            .config
            .roots
            .iter()
            .find(|r| &r.id == id)
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, "git_root", id))?;
        let fs = self
            .config
            .filesystem
            .as_ref()
            .ok_or_else(|| refuse(DomainErrorCode::UnsupportedTarget, "git_filesystem", id))?;
        let path = root
            .path
            .to_str()
            .ok_or_else(|| refuse(DomainErrorCode::UnsupportedTarget, "git_root", id))?;
        let resolved = fs
            .resolve(path, None)
            .map_err(|_| refuse(DomainErrorCode::Conflict, "git_root", id))?;
        if !root.path.is_absolute()
            || resolved.parent_traversal
            || !resolved.linked_ancestors.is_empty()
            || resolved.entry_kind != EntryKind::Directory
            || resolved.target.as_ref().is_none_or(|t| {
                t.path != root.path || !t.is_directory || t.volume_root || t.identity.is_none()
            })
        {
            return Err(refuse(DomainErrorCode::PermissionDenied, "git_root", id));
        }
        crate::services::git::client::path_argument(&root.path)
            .map_err(|_| refuse(DomainErrorCode::UnsupportedTarget, "git_root", id))?;
        Ok(root.path.clone())
    }

    pub(super) fn owned_command(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        id: &Id,
    ) -> Result<(StoredGitWorktree, StoredRepository, StoredWork), Failure> {
        let tree = tx
            .git()
            .git_worktree(id)?
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, "worktree", id))?;
        let work = tx
            .work()
            .work(&tree.work_id)?
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, "work", &tree.work_id))?;
        let repository = tx
            .projects()
            .repository(&tree.repository_id)?
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, "repository", &tree.repository_id))?;
        if ctx.project_id() != Some(&work.project_id) {
            return Err(refuse(DomainErrorCode::NotFound, "worktree", id));
        }
        self.check_work(tx, ctx, &work, &repository)?;
        if tree.removed || tree.pending_operation_id.is_some() || &tree.owner != ctx.principal() {
            return Err(refuse(DomainErrorCode::Conflict, "git_ownership", id));
        }
        Ok((tree, repository, work))
    }

    pub(super) fn check_work(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        work: &StoredWork,
        repository: &StoredRepository,
    ) -> Result<(), Failure> {
        self.check_work_scope(tx, ctx, work, repository, true)
    }
    fn check_work_scope(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        work: &StoredWork,
        repository: &StoredRepository,
        require_assignment: bool,
    ) -> Result<(), Failure> {
        let brief = crate::services::work::model::WorkBrief::of(work)?;
        let project = tx.projects().project(&work.project_id)?;
        let assignment = match ctx.assignment_id() {
            Some(id) => tx.runs().assignment(id)?,
            None => None,
        };
        let delegated_task = if let Some(assignment) = assignment.as_ref()
            && assignment.purpose == AssignmentPurpose::Child
        {
            tx.runs()
                .agent_assignment_context(&assignment.id)?
                .is_some_and(|context| {
                    context.project_id == work.project_id
                        && context.task_ids.as_array().is_some_and(|tasks| {
                            tasks
                                .iter()
                                .any(|task| task.as_str() == Some(work.id.as_str()))
                        })
                })
        } else {
            false
        };
        let assignment_matches = assignment.as_ref().is_some_and(|a| {
            (a.work_id.0 == work.id || delegated_task)
                && Some(&a.root_id) == ctx.root_id()
                && a.state.is_live()
                && Some(&a.session_id) == ctx.session_id()
        });
        let lineage = ctx.root_id().is_some_and(|r| {
            r.0 == work.id
                || (work.parent_feature_id.as_ref() == Some(&r.0)
                    && (!require_assignment || assignment_matches))
        });
        if !matches!(work.kind.as_str(), TASK | FEATURE | "issue")
            || ctx.project_id() != Some(&work.project_id)
            || !lineage
            || repository.lifecycle != ACTIVE
            || project.is_none_or(|p| p.lifecycle != ACTIVE)
            || !tx
                .projects()
                .repository_projects(&repository.id)?
                .contains(&work.project_id)
            || !tx
                .work()
                .work_repositories(&work.id)?
                .contains(&repository.id)
            || (require_assignment
                && ctx.principal_kind() == PrincipalKind::Agent
                && (brief
                    .assigned_agent_id
                    .as_ref()
                    .is_some_and(|a| a != &ctx.principal().0)
                    || brief
                        .assignment_id
                        .as_ref()
                        .is_some_and(|a| Some(a) != ctx.assignment_id())))
        {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                "git_work_scope",
                &work.id,
            ));
        }
        Ok(())
    }

    pub(super) fn validate_new_worktree(
        &self,
        tx: &mut dyn WriteTransaction,
        work: &StoredWork,
        repository: &StoredRepository,
        input: &ops::WorktreeInput,
    ) -> Result<PathBuf, Failure> {
        if input.worktree_id.is_some() {
            return Err(refuse(
                DomainErrorCode::InvalidArgument,
                "new_worktree",
                &work.id,
            ));
        }
        let root = self.root_path(&input.registered_root_id)?;
        if tx
            .authority()
            .registered_repositories()?
            .iter()
            .any(|r| root.starts_with(Path::new(&r.location)))
        {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                "primary_checkout",
                &repository.id,
            ));
        }
        if let Some(feature) = &work.parent_feature_id {
            feature_delivery::require_published_branch(tx, feature, &repository.id, &input.base)?;
            if feature_delivery::uses_local_integration(tx, feature, &repository.id)?
                && !tx.work().open_prerequisites(&work.id)?.is_empty()
            {
                return Err(refuse(
                    DomainErrorCode::Conflict,
                    "task_prerequisites",
                    &work.id,
                ));
            }
        }
        Ok(root)
    }
    fn decide_git(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, Failure> {
        let reviewed_binding =
            crate::services::approvals::owner_preview::approved_binding(tx, ctx, command)?;
        if let Some(binding) = &reviewed_binding {
            let control = self.approval_control.read_locked().clone();
            crate::services::approvals::owner_preview::recheck_control(
                tx,
                ctx,
                control.as_deref(),
            )?;
            self.validate_reviewed_git_command(tx, ctx, command, binding)?;
        }
        let op = OperationId(tx.new_id(IdKind::Operation));
        let mut merge_reservation = None;
        let mut cleanup_proof = None;
        let (mut tree, repository, work, action) = match command.input() {
            OperationInput::RepositoryDocs(
                crate::operations::repository_docs::Input::Generate(input),
            ) => {
                let ownership = ctx.worktree().ok_or_else(|| {
                    refuse(
                        DomainErrorCode::PermissionDenied,
                        "owned_worktree",
                        &input.repository_id,
                    )
                })?;
                let (tree, repository, work) = self.owned_command(tx, ctx, ownership.id())?;
                let prepared = command
                    .prepared::<documentation::PreparedDocs>()
                    .ok_or_else(|| {
                        refuse(
                            DomainErrorCode::Conflict,
                            "documentation_preparation",
                            &tree.id,
                        )
                    })?;
                let draft = prepared.as_ref().map_err(Clone::clone)?.clone();
                if draft.evidence.repository != repository.id
                    || tree.registered_root_id != input.output_root_id
                    || tree.ownership_revision != ownership.ownership_revision()
                    || tree.head != draft.evidence.view.snapshot.head
                    || !draft.evidence.validate_in(tx, ctx)?
                {
                    return Err(refuse(
                        DomainErrorCode::SourceChanged,
                        "documentation_source",
                        &tree.id,
                    ));
                }
                (
                    tree,
                    repository,
                    work,
                    Action::Documentation(Box::new(draft)),
                )
            }
            OperationInput::Git(ops::Input::Worktree(input)) => {
                let work = tx
                    .work()
                    .work(&input.work_id)?
                    .ok_or_else(|| refuse(DomainErrorCode::NotFound, "work", &input.work_id))?;
                if work.revision != input.expected_revision.get() {
                    return Err(refuse(DomainErrorCode::Conflict, "work_revision", &work.id));
                }
                let repository =
                    tx.projects()
                        .repository(&input.repository_id)?
                        .ok_or_else(|| {
                            refuse(
                                DomainErrorCode::NotFound,
                                "repository",
                                &input.repository_id,
                            )
                        })?;
                self.check_work(tx, ctx, &work, &repository)?;
                match input.action {
                    GitActionKind::Create => {
                        let root = self.validate_new_worktree(tx, &work, &repository, input)?;

                        let id = tx.new_id(IdKind::Resource);
                        // IDs do not become path syntax. Hex encoding is reversible and collision free.
                        let component: String =
                            id.as_str().bytes().map(|b| format!("{b:02x}")).collect();
                        let work_component: String = work
                            .id
                            .as_str()
                            .bytes()
                            .map(|b| format!("{b:02x}"))
                            .collect();
                        let path = root
                            .join(component)
                            .to_str()
                            .ok_or_else(|| {
                                refuse(DomainErrorCode::UnsupportedTarget, "git_path", &id)
                            })?
                            .to_owned();
                        let tree = StoredGitWorktree {
                            id,
                            repository_id: repository.id.clone(),
                            work_id: work.id.clone(),
                            owner: ctx.principal().clone(),
                            ownership_revision: 1,
                            owner_root_id: ctx.root_id().cloned(),
                            owner_assignment_id: ctx.assignment_id().cloned(),
                            registered_root_id: input.registered_root_id.clone(),
                            path,
                            branch: if work.kind == FEATURE {
                                format!("refs/heads/feature/{work_component}")
                            } else {
                                format!("refs/heads/devworks/task-{work_component}")
                            },
                            head: input.base.clone(),
                            revision: 1,
                            removed: false,
                            location: None,
                            pending_operation_id: Some(op.clone()),
                            last_operation_id: None,
                            last_result: None,
                        };
                        tx.git().insert_git_worktree(&tree)?;
                        (tree, repository, work, Action::Create)
                    }
                    GitActionKind::Attach | GitActionKind::Inspect => {
                        let id = input.worktree_id.as_ref().ok_or_else(|| {
                            refuse(DomainErrorCode::InvalidArgument, "worktree", &work.id)
                        })?;
                        let (tree, repository, current_work) =
                            if matches!(input.action, GitActionKind::Attach) {
                                self.attach_command(tx, ctx, id)?
                            } else {
                                self.owned_command(tx, ctx, id)?
                            };
                        if current_work.id != work.id
                            || tree.registered_root_id != input.registered_root_id
                            || tree.head != input.base
                        {
                            return Err(refuse(DomainErrorCode::Conflict, "git_ownership", id));
                        }
                        (tree, repository, current_work, Action::Inspect)
                    }
                }
            }
            OperationInput::Git(ops::Input::Commit(input)) => {
                let GitCommand::Commit {
                    worktree_id,
                    expected_head,
                    ..
                } = &input.command
                else {
                    unreachable!("selected command binding")
                };
                let (tree, repository, work) = self.owned_command(tx, ctx, worktree_id)?;
                if &tree.head != expected_head {
                    return Err(refuse(
                        DomainErrorCode::SourceChanged,
                        "git_head",
                        worktree_id,
                    ));
                }
                let message = command.prepared::<StagedMessage>().ok_or_else(|| {
                    refuse(DomainErrorCode::InvalidArgument, "git_message", worktree_id)
                })?;
                if let Some(binding) = &reviewed_binding
                    && message.1.as_ref() != Some(&binding["commitManifest"])
                {
                    return Err(refuse(
                        DomainErrorCode::SourceChanged,
                        "git_commit_contents",
                        worktree_id,
                    ));
                }
                let message = message
                    .0
                    .as_ref()
                    .map_err(|code| refuse(*code, "git_message", worktree_id))?
                    .clone();
                (
                    tree,
                    repository,
                    work,
                    Action::Commit {
                        message,
                        expected: expected_head.clone(),
                    },
                )
            }
            OperationInput::Git(ops::Input::Integrate(input)) => {
                let GitCommand::Integrate { repository_id, .. } = &input.command else {
                    unreachable!("selected final integration");
                };
                let (reservation, work) = delivery::reserve_delivery(tx, ctx, &op, &input.command)?;
                let selected = reservation.repository(repository_id)?;
                let (tree, repository, source) = self.pr_command(tx, ctx, &selected.worktree)?;
                if source != work || repository.revision != selected.registration_revision {
                    return Err(StorageError::ScopeDenied.into());
                }
                merge_reservation = Some(reservation);
                (tree, repository, work, Action::Integrate)
            }
            OperationInput::Git(ops::Input::Cleanup(input)) => {
                let GitCommand::Cleanup {
                    worktree_id,
                    expected_ownership_revision,
                    expected_head,
                } = &input.command
                else {
                    unreachable!("selected cleanup binding")
                };
                let recorded = tx
                    .git()
                    .git_worktree(worktree_id)?
                    .ok_or_else(|| refuse(DomainErrorCode::NotFound, "worktree", worktree_id))?;
                let scoped = tx
                    .work()
                    .work(&recorded.work_id)?
                    .ok_or(StorageError::ScopeDenied)?;
                let (tree, repository, work) = if Self::pr_manager(tx, ctx, &scoped)? {
                    self.pr_command(tx, ctx, worktree_id)?
                } else {
                    self.owned_command(tx, ctx, worktree_id)?
                };
                cleanup_proof = delivery::cleanup_proof(tx, &tree, &work)?;
                if cleanup_proof.is_none() && tree.owner != *ctx.principal() {
                    return Err(refuse(
                        DomainErrorCode::PermissionDenied,
                        "git_ownership",
                        worktree_id,
                    ));
                }
                if expected_ownership_revision.get() != tree.ownership_revision
                    || expected_head != &tree.head
                {
                    return Err(refuse(
                        DomainErrorCode::Conflict,
                        "git_ownership",
                        worktree_id,
                    ));
                }
                if cleanup_proof.is_none()
                    && ctx.assignment_id().is_some()
                    && crate::services::indexing::assigned::owned(tx.git(), ctx)?
                        .is_none_or(|bound| bound.id != tree.id)
                {
                    return Err(refuse(
                        DomainErrorCode::PermissionDenied,
                        "git_ownership",
                        worktree_id,
                    ));
                }
                (tree, repository, work, Action::Cleanup)
            }
            OperationInput::Git(ops::Input::PullRequest(input)) => {
                self.decide_pull_request(ctx, command, tx, &input.command)?
            }
            OperationInput::Git(ops::Input::Push(input)) => {
                self.decide_push(ctx, command, tx, &input.command)?
            }
            _ => {
                return Err(refuse(
                    DomainErrorCode::UnsupportedCapability,
                    "git_command",
                    &Id::new("git").expect("namespace"),
                ));
            }
        };
        let previous_owner = tx
            .git()
            .git_worktree(&tree.id)?
            .filter(|prior| prior.owner != tree.owner)
            .map(|prior| prior.owner);
        if !matches!(action, Action::Create) {
            let expected = tree.revision;
            tree.revision = expected
                .checked_add(1)
                .ok_or_else(|| StorageError::InvalidRecord("Git revision exhausted".into()))?;
            tree.pending_operation_id = Some(op.clone());
            tx.git().update_git_worktree(&tree, expected)?;
        }
        // The worktree fence admitted the previous revision; the reservation above supersedes
        // it. Execution checks that exact reservation plus live work/assignment/memberships.
        let fences = command
            .authorization()
            .fences()
            .iter()
            .filter(|f| !matches!(f, AuthorityFence::GitWorktree { .. }))
            .cloned()
            .collect();
        let queue = tx.new_id(IdKind::QueueEntry);
        let mut affected = vec![RootId(work.id.clone())];
        for root in [
            ctx.root_id().cloned(),
            work.parent_feature_id.clone().map(RootId),
        ]
        .into_iter()
        .flatten()
        {
            if !affected.contains(&root) {
                affected.push(root);
            }
        }
        if let Action::Push {
            task_publication: Some(publication),
            ..
        } = &action
        {
            let task = RootId(publication.proof.task_id.clone());
            if !affected.contains(&task) {
                affected.push(task);
            }
        }
        let root_epochs = affected
            .into_iter()
            .map(|root| {
                tx.runs()
                    .root_control(&root)
                    .map(|control| (root, control.map_or(0, |c| c.stop_epoch)))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let assignment_generation = match ctx.assignment_id() {
            Some(id) => tx.runs().assignment(id)?.map(|a| a.generation),
            None => None,
        };
        let job = Job {
            delivery_id: queue.clone(),
            feature_id: if matches!(
                &action,
                Action::PullRequest {
                    final_feature: true,
                    ..
                }
            ) {
                Some(work.id.clone())
            } else {
                work.parent_feature_id.clone()
            },
            actor: ctx.principal().clone(),
            kind: ctx.principal_kind().as_str().to_string(),
            session: ctx.session_id().cloned(),
            project: work.project_id.clone(),
            root: ctx.root_id().cloned(),
            assignment: ctx.assignment_id().cloned(),
            restore_epoch: ctx.restore_epoch().get(),
            policy_revision: ctx.policy_revision().get(),
            repository_revision: repository.revision,
            work_revision: work.revision,
            root_epochs,
            assignment_generation,
            fences,
            reviewed_binding,
            tree: tree.clone(),
            action,
            cleanup_proof,
        };
        // Cleanup's exact worktree reservation already serializes its native effect and
        // persists after unknown outcomes. A definite refusal releases it, so a fresh cleanup
        // must not wait behind the failed delivery's generic partition hold.
        let partition = (!matches!(job.action, Action::Cleanup | Action::Integrate))
            .then(|| tree.id.as_str().into());
        tx.operations().record_intent(NewOperation {
            id: op.clone(),
            root_id: job.root.clone(),
            assignment_id: job.assignment.clone(),
            capability: command.descriptor().name.into(),
            schema_version: 1,
            args_digest: command.fingerprint().as_str().into(),
            payload: serde_json::to_value(&job)
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
        })?;
        if let Some(mut delivery) = merge_reservation {
            delivery::record_delivery_intent(tx, ctx.principal(), &op, &mut delivery)?;
        }
        if let Some(previous_owner) = previous_owner {
            let event = EventId(tx.new_id(IdKind::Event));
            tx.history().append(NewEvent {
                id: event,
                actor: ctx.principal().clone(),
                project_id: Some(work.project_id.clone()),
                root_id: Some(RootId(work.id.clone())),
                operation_id: Some(op.clone()),
                aggregate_kind: "git_ownership".into(),
                aggregate_id: tree.id.as_str().into(),
                aggregate_revision: tree.ownership_revision,
                kind: "GitWorktreeOwnershipTransferred".into(),
                payload: json!({
                    "repositoryId": tree.repository_id,
                    "workId": tree.work_id,
                    "worktreeId": tree.id,
                    "previousOwner": previous_owner,
                    "owner": tree.owner,
                    "ownershipRevision": tree.ownership_revision,
                    "path": tree.path,
                    "branch": tree.branch
                }),
            })?;
        }
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id: event.clone(),
            actor: ctx.principal().clone(),
            project_id: Some(work.project_id.clone()),
            root_id: ctx.root_id().cloned(),
            operation_id: Some(op.clone()),
            aggregate_kind: "git_worktree".into(),
            aggregate_id: tree.id.as_str().into(),
            aggregate_revision: tree.revision,
            kind: "GitWriteRequested".into(),
            payload: json!({
                "repositoryId": tree.repository_id,
                "workId": tree.work_id,
                "worktreeId": tree.id,
                "branch": tree.branch,
                "expectedHead": tree.head
            }),
        })?;

        let now = tx.now_ms();
        // A human may act on a linked task directly, even when only its feature owns a run.
        let mut delivery_root = ctx.root_id().cloned();
        if ctx.principal_kind() == PrincipalKind::Human
            && ctx.root_id().is_some_and(|root| root.0 == work.id)
            && tx.runs().root_control(&RootId(work.id.clone()))?.is_none()
            && let Some(feature) = &job.feature_id
        {
            delivery_root = Some(RootId(feature.clone()));
        }
        tx.queue().enqueue(NewQueueEntry {
            id: queue,
            consumer: EFFECT_CONSUMER.into(),
            event_id: event,
            root_id: if job.cleanup_proof.is_some() {
                None
            } else {
                delivery_root
            },
            operation_id: Some(op.clone()),
            partition,
            priority: 0,
            ready_at_ms: now,
            payload: json!({"worktreeId":tree.id}),
        })?;
        Ok(Decided {
            decision: DomainDecision::Accepted {
                operation_id: op.clone(),
            },
            operation_id: Some(op),
        })
    }

    fn job(&self, operation: &OperationRecord) -> Result<Job, EffectOutcome> {
        if operation.schema_version != 1 {
            return Err(denied("unsupported journal version"));
        }
        serde_json::from_value(operation.payload.clone())
            .map_err(|_| denied("invalid journal payload"))
    }

    fn execution_target(
        &self,
        job: &Job,
        operation: &OperationId,
    ) -> Result<(InvocationContext, StoredRepository), EffectOutcome> {
        if self.store.info().restore_epoch != job.restore_epoch {
            return Err(EffectOutcome::Unknown(
                "restored store requires observation before any Git write".into(),
            ));
        }
        let ctx = job
            .context(self, operation)
            .ok_or_else(|| denied("invalid principal kind"))?;
        let j = job.clone();
        let c = ctx.clone();
        let repository = run_write(&*self.store, WriteOptions::host_state(), move |tx| {
            let tree = tx
                .git()
                .git_worktree(&j.tree.id)?
                .ok_or(StorageError::ScopeDenied)?;
            if tree != j.tree || !crate::authority::fences_hold(tx, &j.fences)? {
                return Err(StorageError::ScopeDenied);
            }
            let work = tx
                .work()
                .work(&tree.work_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let repository = tx
                .projects()
                .repository(&tree.repository_id)?
                .ok_or(StorageError::ScopeDenied)?;
            if let Action::Push {
                task_publication: Some(task),
                ..
            } = &j.action
            {
                feature_delivery::publication_current(tx, &work, &j.tree, task)?;
            }
            // Domain scope checks below run on the same current snapshot as the reservation.
            if work.revision != j.work_revision || repository.revision != j.repository_revision {
                return Err(StorageError::ScopeDenied);
            }
            let brief = crate::services::work::model::WorkBrief::of(&work)?;
            let managing = (matches!(j.action, Action::PullRequest { .. } | Action::Integrate)
                || (matches!(j.action, Action::Cleanup) && j.cleanup_proof.is_some()))
                && Self::pr_manager(tx, &c, &work)?;
            if let Action::PullRequest {
                covered,
                final_feature,
                integrated_heads,
                ..
            } = &j.action
            {
                if *final_feature {
                    let coverage = feature_delivery::final_pr_coverage(tx, &work, &tree)
                        .map_err(Failure::into_storage)?;
                    if coverage
                        .iter()
                        .map(|(id, rev, _)| (id, *rev))
                        .ne(covered.iter().map(|(id, rev)| (id, *rev)))
                        || coverage
                            .iter()
                            .map(|(_, _, head)| head)
                            .ne(integrated_heads)
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                }
                for (id, revision) in covered {
                    let covered_work = tx.work().work(id)?.ok_or(StorageError::ScopeDenied)?;
                    if covered_work.revision != *revision
                        || covered_work.project_id != j.project
                        || covered_work.parent_feature_id != j.feature_id
                        || !tx.work().work_repositories(id)?.contains(&repository.id)
                    {
                        return Err(StorageError::ScopeDenied);
                    }
                }
            }
            if tree.removed
                || (!managing && tree.owner != j.actor)
                || c.project_id() != Some(&work.project_id)
                || repository.lifecycle != ACTIVE
                || tx
                    .projects()
                    .project(&work.project_id)?
                    .is_none_or(|p| p.lifecycle != ACTIVE)
                || !tx
                    .work()
                    .work_repositories(&work.id)?
                    .contains(&repository.id)
                || !tx
                    .projects()
                    .repository_projects(&repository.id)?
                    .contains(&work.project_id)
                || (!managing
                    && c.principal_kind() == PrincipalKind::Agent
                    && (brief
                        .assigned_agent_id
                        .as_ref()
                        .is_some_and(|a| a != &j.actor.0)
                        || brief
                            .assignment_id
                            .as_ref()
                            .is_some_and(|a| Some(a) != c.assignment_id())))
            {
                return Err(StorageError::ScopeDenied);
            }
            Ok(repository)
        })
        .map_err(|_| denied("ownership, assignment or authority changed"))?;
        Ok((ctx, repository))
    }

    fn finish_git(
        &self,
        job: &Job,
        operation: &OperationId,
        observation: crate::services::git::client::RevisionObservation,
        published_docs: Option<Value>,
    ) -> EffectOutcome {
        if observation.branch.as_deref() != Some(&job.tree.branch) {
            return EffectOutcome::Unknown("owned branch changed".into());
        }
        if matches!(job.action, Action::Create)
            && job.reviewed_binding.is_some()
            && (observation.head != job.tree.head.hex || observation.dirty)
        {
            return EffectOutcome::Unknown(
                "reviewed worktree checkout is not complete at its exact base".into(),
            );
        }
        let repository = match read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            snapshot.projects().repository(&job.tree.repository_id)
        }) {
            Ok(Some(repo)) => repo,
            _ => {
                return EffectOutcome::Unknown(
                    "repository identity is unavailable after Git effect".into(),
                );
            }
        };
        let Some(ctx) = job.context(self, operation) else {
            return denied("invalid principal kind");
        };
        let stop = || false;
        let helpers = crate::services::git::helper::RecordedHelpers {
            inner: &*self.helpers,
            operation,
        };
        let git = Git {
            runner: &helpers,
            ctx: &ctx,
            program: &self.config.program,
            deadline: self.config.deadline,
            stop: &stop,
        };
        let location = match self.resolve_location(&git, &repository, &job.tree) {
            Ok(location)
                if job
                    .tree
                    .location
                    .as_ref()
                    .is_none_or(|old| old == &location) =>
            {
                location
            }
            _ => {
                return EffectOutcome::Unknown(
                    "physical Git ownership changed during the effect".into(),
                );
            }
        };
        if job.tree.location.is_none() && !matches!(job.action, Action::Create) {
            return EffectOutcome::Unknown("worktree has no physical ownership witness".into());
        }
        let head = oid(observation.head);
        let value = GitInspection {
            repository_id: job.tree.repository_id.clone(),
            worktree_id: Some(job.tree.id.clone()),
            head: head.clone(),
            branch: Some(crate::protocol::support::BoundedText(
                job.tree.branch.clone(),
            )),
            dirty: observation.dirty,
            state: OperationState::ObservedComplete,
            evidence: crate::protocol::support::BoundedList(vec![]),
        };
        let result = match &job.action {
            Action::Documentation(_) => OperationResult::RepositoryDocsGenerate {
                value: OperationRef {
                    operation_id: operation.clone(),
                    state: OperationState::ObservedComplete,
                },
            },
            Action::Commit { .. } => OperationResult::GitCommit { value },
            Action::Push { .. } => OperationResult::GitPush { value },
            Action::Cleanup => unreachable!("cleanup settles without a live checkout"),
            Action::PullRequest { .. } | Action::Integrate => {
                return EffectOutcome::Unknown(
                    "PR completion requires a remote observation".into(),
                );
            }
            _ => OperationResult::GitWorktree { value },
        };
        use crate::protocol::records::RepositoryDocsDelivery;
        let staged_message = if matches!(&job.action, Action::Documentation(draft)
            if matches!(draft.delivery, Some(RepositoryDocsDelivery::Commit | RepositoryDocsDelivery::CommitAndPush)))
        {
            match self.artifacts.stage_host_bytes(
                &*self.store,
                b"Update repository documentation\n",
                json!({"kind":"RepositoryDocumentationCommitMessage","operationId":operation}),
            ) {
                Ok(Ok(staged)) => Some(staged),
                _ => {
                    return EffectOutcome::Unknown(
                        "documentation commit message could not be staged".into(),
                    );
                }
            }
        } else {
            None
        };
        let message = staged_message.clone();
        let artifacts = self.artifacts.clone();
        let j = job.clone();
        let op = operation.clone();
        let checkpoint = json!({"result":result});
        let durable = checkpoint.clone();
        let outcome = run_write(&*self.store, WriteOptions::settlement(), move |tx| {
            let current = tx
                .git()
                .git_worktree(&j.tree.id)?
                .ok_or(StorageError::ScopeDenied)?;
            if current.last_operation_id.as_ref() == Some(&op)
                && current.pending_operation_id.is_none()
                && current.head == head
            {
                return Ok(());
            }
            if current != j.tree {
                return Err(StorageError::ScopeDenied);
            }
            let mut updated = current.clone();
            updated.head = head.clone();
            updated.location = Some(location.clone());
            updated.pending_operation_id = None;
            updated.last_operation_id = Some(op.clone());
            updated.last_result = Some(durable.clone());
            updated.revision += 1;
            tx.git().update_git_worktree(&updated, current.revision)?;
            if matches!(j.action, Action::Create | Action::Inspect) {
                crate::services::indexing::register_git_worktree(tx, &updated)?;
            }
            let id = EventId(tx.new_id(IdKind::Event));
            let sequence = tx.history().append(NewEvent {
                id: id.clone(),
                actor: j.actor.clone(),
                project_id: Some(j.project.clone()),
                root_id: j.root.clone(),
                operation_id: Some(op.clone()),
                aggregate_kind: "git_worktree".into(),
                aggregate_id: updated.id.as_str().into(),
                aggregate_revision: updated.revision,
                kind: "GitWriteObserved".into(),
                payload: json!({
                    "repositoryId": updated.repository_id,
                    "workId": updated.work_id,
                    "worktreeId": updated.id,
                    "path": updated.path,
                    "branch": updated.branch,
                    "head": head
                }),
            })?;
            if let Action::Documentation(draft) = &j.action {
                let event = EventId(tx.new_id(IdKind::Event));
                let commit_message = message
                    .as_ref()
                    .map(|staged| {
                        let version = artifacts.commit_host_bytes_in_scope(
                            tx,
                            staged,
                            crate::services::artifacts::host::HostArtifactPublication {
                                title: "Repository documentation commit message",
                                media_type: "text/plain; charset=utf-8",
                                provenance: json!({"operationId":op}),
                                scope: crate::ports::storage::ArtifactScope::Project(
                                    j.project.clone(),
                                ),
                                owner: j.actor.clone(),
                            },
                        )?;
                        Ok::<_, StorageError>(crate::services::artifacts::version_ref(&version))
                    })
                    .transpose()?;
                let evidence = json!({
                    "repositoryId": updated.repository_id,
                    "workId": updated.work_id,
                    "worktreeId": updated.id,
                    "analysisRunId": draft.evidence.run,
                    "analysisRevision": draft.evidence.revision,
                    "viewId": draft.evidence.view.view_id,
                    "viewDigest": draft.evidence.view.view_digest,
                    "sourceHead": head,
                    "paths": draft.files.keys().collect::<Vec<_>>(),
                    "files": draft.files.iter().map(|(path, text)| json!({
                        "path": path, "sha256": crate::fingerprint::sha256_hex(text.as_bytes())
                    })).collect::<Vec<_>>(),
                    "missingProjects": draft.missing_projects,
                    "complete": draft.complete,
                    "delivery": draft.delivery,
                    "commitMessage": commit_message
                });
                tx.history().append(NewEvent {
                    id: event.clone(),
                    actor: j.actor.clone(),
                    project_id: Some(j.project.clone()),
                    root_id: j.root.clone(),
                    operation_id: Some(op.clone()),
                    aggregate_kind: "repository_documentation".into(),
                    aggregate_id: updated.id.as_str().into(),
                    aggregate_revision: updated.revision,
                    kind: "RepositoryDocumentationGenerated".into(),
                    payload: evidence,
                })?;
                if message.is_some() {
                    documentation::request(tx, event, &j)?;
                }
            }
            let mut documentation = None;
            if let Some(evidence) = published_docs {
                let (kind, requests_push, qualifies) = match j.action {
                    Action::Commit { .. } => (
                        "RepositoryDocumentationCommitted",
                        evidence["delivery"] == "CommitAndPush"
                            && j.reviewed_binding.as_ref().is_some_and(|binding| {
                                binding["artifact"]["reference"] == evidence["commitMessage"]
                            }),
                        false,
                    ),
                    Action::Push { .. } => (
                        "RepositoryDocumentationPublished",
                        false,
                        evidence["complete"] == true,
                    ),
                    _ => return Err(StorageError::ScopeDenied),
                };
                let event = EventId(tx.new_id(IdKind::Event));
                let sequence = tx.history().append(NewEvent {
                    id: event.clone(),
                    actor: j.actor.clone(),
                    project_id: Some(j.project.clone()),
                    root_id: j.root.clone(),
                    operation_id: Some(op.clone()),
                    aggregate_kind: "repository_documentation".into(),
                    aggregate_id: updated.id.as_str().into(),
                    aggregate_revision: updated.revision,
                    kind: kind.into(),
                    payload: evidence.clone(),
                })?;
                if requests_push {
                    documentation::request(tx, event.clone(), &j)?;
                }
                if qualifies {
                    documentation = Some((event, sequence, evidence));
                }
            }
            let mut roots = vec![RootId(updated.work_id.clone())];
            if let Some(feature) = &j.feature_id {
                roots.push(RootId(feature.clone()));
            }
            for root in roots {
                if let Some((event, doc_sequence, evidence)) = &documentation {
                    let link = tx.new_id(IdKind::Link);
                    let created_at_ms = tx.now_ms();
                    tx.sessions().insert_link(&StoredLink {
                        id: link,
                        project_id: j.project.clone(),
                        root_id: root.clone(),
                        kind: "evidence".into(),
                        target_kind: "repository_documentation".into(),
                        target_id: op.0.as_str().into(),
                        parent_id: None,
                        supersedes_id: None,
                        actor: j.actor.clone(),
                        event_id: event.0.clone(),
                        sequence: *doc_sequence,
                        repository_id: Some(updated.repository_id.clone()),
                        revision: Some(head.hex.clone()),
                        summary: "Published repository documentation with verified contents".into(),
                        body: evidence.clone(),
                        created_at_ms,
                    })?;
                }
                let link = tx.new_id(IdKind::Link);
                let now = tx.now_ms();
                tx.sessions().insert_link(&StoredLink {
                    id: link,
                    project_id: j.project.clone(),
                    root_id: root,
                    kind: "git".into(),
                    target_kind: "git_worktree".into(),
                    target_id: updated.id.as_str().into(),
                    parent_id: None,
                    supersedes_id: None,
                    actor: j.actor.clone(),
                    event_id: id.0.clone(),
                    sequence,
                    repository_id: Some(updated.repository_id.clone()),
                    revision: Some(head.hex.clone()),
                    summary: format!("{} at {}", updated.branch, updated.path),
                    body: json!({
                        "agentSessionId": j.session,
                        "worktreeId": updated.id,
                        "path": updated.path,
                        "branch": updated.branch,
                        "head": head
                    }),
                    created_at_ms: now,
                })?;
            }
            if let Action::Push {
                task_publication: Some(task),
                ..
            } = &j.action
            {
                feature_delivery::settle_task_publication(tx, &ctx, &updated, &op, task)?;
            }
            Ok(())
        });
        if let Some(staged) = staged_message.as_ref() {
            match &outcome {
                Ok(()) => self.artifacts.settle_host_bytes(&*self.store, staged, true),
                Err(StorageError::CommitUnknown) => {}
                Err(_) => self
                    .artifacts
                    .settle_host_bytes(&*self.store, staged, false),
            }
        }
        match outcome {
            Ok(()) => EffectOutcome::Completed(checkpoint),
            Err(e) => storage_failure(e),
        }
    }

    fn execute_cleanup(
        &self,
        job: &Job,
        operation: &OperationRecord,
        git: &Git<'_>,
        repository: &StoredRepository,
    ) -> EffectOutcome {
        let path = Path::new(&job.tree.path);
        let repo = Path::new(&repository.location);
        if !repository.primary
            || self.root_path(&job.tree.registered_root_id).ok().as_deref() != path.parent()
            || self.same_worktree_path(repo, path)
        {
            return denied("cleanup target is not a registered isolated worktree");
        }
        if let Err(reason) = require_clean_checkout(git, &job.tree, path) {
            return changed(reason);
        }
        if let Some(proof) = &job.cleanup_proof {
            if let Err(reason) =
                self.verify_delivery_cleanup(job, operation, git, repository, proof)
            {
                return reason;
            }
        } else {
            let default = match git.default_branch(repo) {
                Ok(Some(branch)) => branch
                    .strip_prefix("refs/heads/")
                    .map(|name| format!("refs/remotes/origin/{name}")),
                Ok(None) if matches!(git.origin(repo), Ok(None)) => {
                    git.branch(repo).ok().flatten().filter(|name| {
                        matches!(name.as_str(), "refs/heads/main" | "refs/heads/master")
                    })
                }
                _ => None,
            };
            let base = default
                .as_deref()
                .and_then(|name| git.resolve(repo, name).ok().flatten())
                .map(|(head, _)| head);
            if !base.as_deref().is_some_and(|head| {
                matches!(git.is_ancestor(repo, &job.tree.head.hex, head), Ok(true))
            }) {
                return denied("cleanup requires the exact worktree head to be integrated");
            }
        }
        let Some(staged) = cleanup_stage(path, &operation.id) else {
            return denied("cleanup worktree path is unsupported");
        };
        let (Ok(original), Ok(staging)) = (
            crate::services::git::client::path_argument(path),
            crate::services::git::client::path_argument(&staged),
        ) else {
            return denied("cleanup worktree path is unsupported");
        };
        if !self.cleanup_path_missing(&staged) {
            return changed("the cleanup staging path changed");
        }
        if let Err(reason) = require_clean_checkout(git, &job.tree, path) {
            return changed(reason);
        }
        if self.git_stopped(job)
            || job
                .cleanup_proof
                .as_ref()
                .is_some_and(|proof| !self.cleanup_writers_quiesced(job, operation, proof))
        {
            return EffectOutcome::NotApplied(
                "cleanup authority or writers changed before staging".into(),
            );
        }
        let move_effect = git.mutation(
            repo,
            &["worktree", "move", "--", &original, &staging],
            None,
            &operation.id,
            false,
        );
        if move_effect.is_err() {
            if definite_git_refusal(&move_effect)
                && self.cleanup_path_missing(&staged)
                && require_clean_checkout(git, &job.tree, path).is_ok()
            {
                return changed("Git refused to move the intact checkout");
            }
            return EffectOutcome::Unknown(
                "the checkout could not be safely staged; no removal was attempted".into(),
            );
        }
        let restore = || {
            if !self.cleanup_path_missing(path)
                || !cleanup_identity_matches(git, &job.tree, &staged)
            {
                return false;
            }
            let _ = git.mutation(
                repo,
                &["worktree", "move", "--", &staging, &original],
                None,
                &operation.id,
                false,
            );
            self.cleanup_path_missing(&staged) && cleanup_identity_matches(git, &job.tree, path)
        };
        // Moving the physical checkout first fences path-based writers from the deletion target.
        // Recheck the exact native witness, ref and all local data at the staged path.
        if require_clean_checkout(git, &job.tree, &staged).is_err()
            || !self.cleanup_path_missing(path)
        {
            return if restore() {
                changed("the staged checkout had changed and was restored with its data")
            } else {
                EffectOutcome::Unknown("staged checkout retained for recovery".into())
            };
        }
        if job.cleanup_proof.is_some() {
            return self.retire_delivery_worktree(job, operation, git, repository, &staged);
        }
        let removal = git.mutation(
            repo,
            &["worktree", "remove", "--", &staging],
            None,
            &operation.id,
            false,
        );
        if definite_git_refusal(&removal)
            && require_clean_checkout(git, &job.tree, &staged).is_ok()
            && restore()
        {
            return changed("Git refused to remove the intact checkout");
        }
        self.observe_cleanup(job, operation, git, repository)
    }

    fn verify_delivery_cleanup(
        &self,
        job: &Job,
        operation: &OperationRecord,
        git: &Git<'_>,
        repository: &StoredRepository,
        proof: &delivery::CleanupProof,
    ) -> Result<(), EffectOutcome> {
        if !self.cleanup_writers_quiesced(job, operation, proof) {
            return Err(EffectOutcome::NotApplied(
                "owned writers are still quiescing".into(),
            ));
        }
        let repo = Path::new(&repository.location);
        if git.is_ancestor(repo, &proof.integrated_candidate.hex, &proof.final_head.hex) != Ok(true)
            || !self.cleanup_repository_matches(job, git, repository)
        {
            return Err(denied(
                "integrated worktree identity cannot be verified for cleanup",
            ));
        }
        use super::github::{Fault, Repository};
        let client = self
            .config
            .github
            .as_ref()
            .ok_or_else(|| denied("GitHub is unavailable for cleanup"))?;
        let target = repository
            .remote_url
            .as_deref()
            .and_then(Repository::from_remote)
            .ok_or_else(|| denied("GitHub repository changed before cleanup"))?;
        let main = client
            .branch_head(self.store.as_ref(), &target, "main")
            .and_then(|main| {
                client.commit_reaches_main(
                    self.store.as_ref(),
                    &target,
                    &proof.merge_commit.hex,
                    &main,
                )
            });
        match main {
            Ok(true) => {}
            Err(Fault::Unavailable) => {
                return Err(EffectOutcome::NotApplied(
                    "GitHub could not be reached for cleanup verification".into(),
                ));
            }
            _ => {
                return Err(denied(
                    "current main does not prove the verified merge for cleanup",
                ));
            }
        }
        Ok(())
    }

    fn cleanup_repository_matches(
        &self,
        job: &Job,
        git: &Git<'_>,
        repository: &StoredRepository,
    ) -> bool {
        let path = Path::new(&job.tree.path);
        if !repository.primary
            || self.root_path(&job.tree.registered_root_id).ok().as_deref() != path.parent()
        {
            return false;
        }
        let observed = (|| {
            let (_, common) = git.metadata_dirs(Path::new(&repository.location)).ok()?;
            self.config
                .filesystem
                .as_ref()?
                .resolve(&common, None)
                .ok()?
                .target?
                .identity
        })();
        observed.is_some_and(|identity| {
            job.tree
                .location
                .as_ref()
                .is_some_and(|location| location.common_dir == identity)
        })
    }

    fn resume_delivery_cleanup(
        &self,
        job: &Job,
        operation: &OperationRecord,
        git: &Git<'_>,
        repository: &StoredRepository,
    ) -> EffectOutcome {
        let Some(proof) = &job.cleanup_proof else {
            return denied("cleanup delivery evidence unavailable");
        };
        let path = Path::new(&job.tree.path);
        let Some(staged) = cleanup_stage(path, &operation.id) else {
            return denied("unsupported cleanup path");
        };
        if let Err(reason) = self.verify_delivery_cleanup(job, operation, git, repository, proof) {
            return EffectOutcome::Unknown(format!("cleanup cannot resume safely: {reason:?}"));
        }
        if self.cleanup_path_missing(&staged) {
            return self.observe_cleanup(job, operation, git, repository);
        }
        self.retire_delivery_worktree(job, operation, git, repository, &staged)
    }

    fn cleanup_writers_quiesced(
        &self,
        job: &Job,
        operation: &OperationRecord,
        proof: &delivery::CleanupProof,
    ) -> bool {
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), |snapshot| {
            let Some(control) = snapshot.runs().root_control(&proof.root)? else {
                return Ok(false);
            };
            if control.desired != DesiredExecution::Paused
                || !auto_cleanup::stop_verified(snapshot.operations(), &control)?
            {
                return Ok(false);
            }
            let mut roots = vec![proof.root.clone(), RootId(job.tree.work_id.clone())];
            if let Some(owner) = &job.tree.owner_assignment_id
                && let Some(assignment) = snapshot.runs().assignment(owner)?
                && assignment.state.is_live()
                && !roots.contains(&assignment.root_id)
            {
                roots.push(assignment.root_id);
            }
            for root in &roots {
                if snapshot.runs().live_activation(root)?.is_some() {
                    return Ok(false);
                }
                if root != &proof.root
                    && let Some(control) = snapshot.runs().root_control(root)?
                    && control.stop_operation_id.is_some()
                    && !auto_cleanup::stop_verified(snapshot.operations(), &control)?
                {
                    return Ok(false);
                }
                let processes = snapshot.resources().processes(
                    &ProcessFilter::UnsettledRoot(root.clone()),
                    None,
                    MAX_PAGE,
                )?;
                if processes.len() == usize::from(MAX_PAGE)
                    || processes
                        .iter()
                        .any(|p| p.operation_id.as_ref() != Some(&operation.id))
                {
                    return Ok(false);
                }
            }
            Ok(true)
        })
        .unwrap_or(false)
    }

    fn delivery_staged_clean(&self, job: &Job, git: &Git<'_>, staged: &Path) -> bool {
        cleanup_identity_matches(git, &job.tree, staged)
            && git
                .reviewed_revision_observation(staged)
                .is_ok_and(|observation| {
                    observation.head == job.tree.head.hex
                        && !observation.dirty
                        && observation
                            .branch
                            .as_deref()
                            .is_none_or(|branch| branch == job.tree.branch.as_str())
                })
            && git.has_status_hidden_entries(staged) == Ok(false)
            && git.has_ignored_files(staged) == Ok(false)
    }

    fn retire_delivery_worktree(
        &self,
        job: &Job,
        operation: &OperationRecord,
        git: &Git<'_>,
        repository: &StoredRepository,
        staged: &Path,
    ) -> EffectOutcome {
        let original = Path::new(&job.tree.path);
        let repo = Path::new(&repository.location);
        let unknown = || {
            EffectOutcome::Unknown(
                "staged checkout or branch changed; local data was retained".into(),
            )
        };
        let stopped = || {
            self.git_stopped(job)
                || job
                    .cleanup_proof
                    .as_ref()
                    .is_none_or(|proof| !self.cleanup_writers_quiesced(job, operation, proof))
        };
        if !self.cleanup_path_missing(original)
            || !self.delivery_staged_clean(job, git, staged)
            || stopped()
        {
            return unknown();
        }
        let Ok(branch) = git.branch(staged) else {
            return unknown();
        };
        if branch.is_some() {
            let detached = (|| {
                let policy = git.reviewed_commit_policy(staged)?;
                git.reviewed_mutation(
                    staged,
                    &["checkout", "--detach", &job.tree.head.hex],
                    None,
                    &operation.id,
                    &policy,
                )?;
                git.branch(staged)
            })();
            if detached != Ok(None) {
                return unknown();
            }
        }
        if !self.cleanup_path_missing(original)
            || !self.delivery_staged_clean(job, git, staged)
            || !git.worktrees(repo).is_ok_and(|trees| {
                !trees
                    .iter()
                    .any(|tree| tree.branch.as_deref() == Some(&job.tree.branch))
            })
            || stopped()
        {
            return unknown();
        }
        match git.resolve(staged, &job.tree.branch) {
            Ok(Some((head, _))) if head == job.tree.head.hex => {
                let allowed =
                    read_snapshot(self.store.as_ref(), &ReadOptions::default(), |snapshot| {
                        let protected = crate::authority::policy::repository_protected_branches(
                            snapshot,
                            &job.tree.repository_id,
                        )?;
                        Ok(!crate::authority::policy::protected_ref(
                            &job.tree.branch,
                            &protected,
                        ))
                    });
                if !matches!(allowed, Ok(true))
                    || git
                        .retire_owned_branch(
                            staged,
                            &job.tree.branch,
                            &job.tree.head.hex,
                            &operation.id,
                        )
                        .is_err()
                {
                    return unknown();
                }
            }
            Ok(None) => {}
            _ => return unknown(),
        }
        let Ok(staging) = crate::services::git::client::path_argument(staged) else {
            return unknown();
        };
        if !self.cleanup_path_missing(original)
            || !self.delivery_staged_clean(job, git, staged)
            || git.resolve(staged, &job.tree.branch) != Ok(None)
            || stopped()
        {
            return unknown();
        }
        let _ = git.mutation(
            repo,
            &["worktree", "remove", "--", &staging],
            None,
            &operation.id,
            false,
        );
        self.observe_cleanup(job, operation, git, repository)
    }

    fn cleanup_path_missing(&self, path: &Path) -> bool {
        self.config.filesystem.as_ref().is_some_and(|fs| {
            path.to_str()
                .and_then(|path| fs.resolve(path, None).ok())
                .is_some_and(|entry| {
                    entry.entry_kind == EntryKind::Missing && entry.linked_ancestors.is_empty()
                })
        })
    }

    fn observe_cleanup(
        &self,
        job: &Job,
        operation: &OperationRecord,
        git: &Git<'_>,
        repository: &StoredRepository,
    ) -> EffectOutcome {
        let path = Path::new(&job.tree.path);
        let repo = Path::new(&repository.location);
        if job.cleanup_proof.is_some()
            && self.cleanup_path_missing(path)
            && self.cleanup_repository_matches(job, git, repository)
            && let Some(staged) = cleanup_stage(path, &operation.id)
            && self.delivery_staged_clean(job, git, &staged)
        {
            return EffectOutcome::NotApplied(
                "exact unchanged checkout is staged for removal".into(),
            );
        }
        let absence = (|| {
            let fs = self.config.filesystem.as_ref()?;
            let (_, common) = git.metadata_dirs(repo).ok()?;
            let identity = fs.resolve(&common, None).ok()?.target?.identity?;
            if identity != job.tree.location.as_ref()?.common_dir
                || self.root_path(&job.tree.registered_root_id).ok().as_deref() != path.parent()
            {
                return None;
            }
            let staged = cleanup_stage(path, &operation.id)?;
            if !self.cleanup_path_missing(path)
                || !self.cleanup_path_missing(&staged)
                || git.worktrees(repo).ok()?.iter().any(|tree| {
                    let registered = Path::new(&tree.path);
                    self.same_worktree_path(registered, path)
                        || self.same_worktree_path(registered, &staged)
                })
                || match git.resolve(repo, &job.tree.branch).ok()? {
                    None => job.cleanup_proof.is_none(),
                    Some((head, _)) => job.cleanup_proof.is_some() || head != job.tree.head.hex,
                }
            {
                return None;
            }
            Some(())
        })();
        if absence.is_none() {
            return EffectOutcome::Unknown(
                "worktree removal could not be established; no write was repeated".into(),
            );
        }
        let job = job.clone();
        let operation = operation.id.clone();
        let outcome = run_write(&*self.store, WriteOptions::settlement(), move |tx| {
            let mut tree = tx
                .git()
                .git_worktree(&job.tree.id)?
                .ok_or(StorageError::ScopeDenied)?;
            if tree.removed && tree.last_operation_id.as_ref() == Some(&operation) {
                return tree.last_result.ok_or(StorageError::CommitUnknown);
            }
            if tree != job.tree || tree.pending_operation_id.as_ref() != Some(&operation) {
                return Err(StorageError::ScopeDenied);
            }
            let prior = tree.revision;
            tree.revision = prior
                .checked_add(1)
                .ok_or_else(|| StorageError::InvalidRecord("Git revision exhausted".into()))?;
            tree.removed = true;
            tree.pending_operation_id = None;
            tree.last_operation_id = Some(operation.clone());
            let result = json!({"result": OperationResult::GitCleanup {
                value: crate::protocol::records::MutationSummary {
                    id: tree.id.clone(), revision: crate::services::kernel::rev(tree.revision), changed: true
                },
            }});
            tree.last_result = Some(result.clone());
            crate::services::indexing::retire_git_worktree(tx, &tree, &job.actor, &job.project)?;
            tx.git().update_git_worktree(&tree, prior)?;
            let id = EventId(tx.new_id(IdKind::Event));
            tx.history().append(NewEvent {
                id,
                actor: job.actor,
                project_id: Some(job.project),
                root_id: job.root,
                operation_id: Some(operation),
                aggregate_kind: "git_worktree".into(),
                aggregate_id: tree.id.as_str().to_owned(),
                aggregate_revision: tree.revision,
                kind: "GitWorktreeRemoved".into(),
                payload: json!({"repositoryId": tree.repository_id, "workId": tree.work_id}),
            })?;
            Ok(result)
        });
        match outcome {
            Ok(result) => EffectOutcome::Completed(result),
            Err(error) => storage_failure(error),
        }
    }

    fn git_stopped(&self, job: &Job) -> bool {
        if self.is_stopped() {
            return true;
        }
        read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let authorization = crate::ports::Authorization::direct().fenced_by(job.fences.clone());
            if !crate::authority::authorization_holds(snapshot, &authorization)?
                || snapshot
                    .projects()
                    .repository(&job.tree.repository_id)?
                    .is_none_or(|repository| {
                        repository.lifecycle != ACTIVE
                            || repository.revision != job.repository_revision
                    })
                || snapshot.git().git_worktree(&job.tree.id)?.as_ref() != Some(&job.tree)
                || snapshot
                    .work()
                    .work(&job.tree.work_id)?
                    .is_none_or(|work| work.revision != job.work_revision)
                || snapshot
                    .queue()
                    .entry(&job.delivery_id)?
                    .is_none_or(|entry| entry.cancel_requested)
            {
                return Ok(true);
            }
            if let Some(assignment) = &job.assignment
                && snapshot.runs().assignment(assignment)?.is_none_or(|a| {
                    !a.state.is_live() || Some(a.generation) != job.assignment_generation
                })
            {
                return Ok(true);
            }
            if let Action::Push {
                task_publication: Some(publication),
                ..
            } = &job.action
                && !publication.current_in(snapshot, job)?
            {
                return Ok(true);
            }
            if let Action::PullRequest { covered, .. } = &job.action {
                for (id, revision) in covered {
                    if snapshot.work().work(id)?.is_none_or(|work| {
                        work.revision != *revision
                            || work.project_id != job.project
                            || work.parent_feature_id != job.feature_id
                    }) || !snapshot
                        .work()
                        .work_repositories(id)?
                        .contains(&job.tree.repository_id)
                    {
                        return Ok(true);
                    }
                }
            }
            for (root, epoch) in &job.root_epochs {
                if let Some(control) = snapshot.runs().root_control(root)?
                    && ((control.desired == DesiredExecution::Paused
                        && job.cleanup_proof.is_none())
                        || (control.stop_operation_id.is_some()
                            && (job.cleanup_proof.is_none()
                                || !auto_cleanup::stop_verified(snapshot.operations(), &control)?))
                        || control.stop_epoch != *epoch)
                {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .unwrap_or(true)
    }

    /// Called once only after the dispatcher's durable Dispatching transition. A failed
    /// mutating helper is conservative: it may have changed the index, refs or worktree.
    pub fn execute(&self, operation: &OperationRecord) -> EffectOutcome {
        let result = self.execute_once(operation);
        if let EffectOutcome::Failed(reason) = &result {
            let job = match self.job(operation) {
                Ok(job) => job,
                Err(_) => return result,
            };
            if let Err(error) = self.settle_denial(&job, &operation.id, reason) {
                return storage_failure(error);
            }
        }
        result
    }

    fn settle_denial(
        &self,
        job: &Job,
        operation: &OperationId,
        reason: &str,
    ) -> Result<(), StorageError> {
        let (job, operation, reason) = (job.clone(), operation.clone(), reason.to_owned());
        run_write(&*self.store, WriteOptions::settlement(), move |tx| {
            let Some(mut tree) = tx.git().git_worktree(&job.tree.id)? else {
                return Ok(());
            };
            if tree.pending_operation_id.as_ref() != Some(&operation) {
                return Ok(());
            }
            if matches!(job.action, Action::Integrate) {
                delivery::release_refused_delivery(tx, &job, &operation)?;
            }
            let prior = tree.revision;
            tree.revision += 1;
            tree.pending_operation_id = None;
            tree.last_operation_id = Some(operation.clone());
            tree.last_result = Some(json!({"failed":reason}));
            // A creation refused before dispatch owns no local resource. Keep its historical
            // identity, but release the branch reservation without touching any filesystem path.
            if matches!(job.action, Action::Create) {
                tree.removed = true;
            }
            tx.git().update_git_worktree(&tree, prior)?;
            let event = EventId(tx.new_id(IdKind::Event));
            tx.history().append(NewEvent {
                id: event,
                actor: job.actor.clone(),
                project_id: Some(job.project.clone()),
                root_id: job.root.clone(),
                operation_id: Some(operation.clone()),
                aggregate_kind: "git_worktree".into(),
                aggregate_id: tree.id.as_str().into(),
                aggregate_revision: tree.revision,
                kind: "GitWriteRefusedBeforeDispatch".into(),
                payload: json!({
                    "repositoryId": tree.repository_id,
                    "workId": tree.work_id,
                    "reason": reason
                }),
            })?;
            Ok(())
        })
    }

    /// Release only reservations whose journal proves cancellation preceded native dispatch.
    /// Dispatched or unknown effects retain their reservation and are never repeated here.
    pub fn reconcile_cancelled(&self) -> Result<usize, StorageError> {
        let mut after = None;
        let mut settled = 0;
        loop {
            let rows = read_snapshot(&*self.store, &ReadOptions::default(), |s| {
                s.git().git_pending_worktrees(after.as_ref(), MAX_PAGE)
            })?;
            if rows.is_empty() {
                break;
            }
            for tree in &rows {
                let Some(id) = &tree.pending_operation_id else {
                    continue;
                };
                let operation = read_snapshot(&*self.store, &ReadOptions::default(), |s| {
                    s.operations().operation(id)
                })?;
                if let Some(operation) = operation
                    && operation.state == OperationState::CancelledBeforeDispatch
                {
                    let job = self.job(&operation).map_err(|_| {
                        StorageError::InvalidRecord("invalid cancelled Git journal".into())
                    })?;
                    self.settle_denial(&job, &operation.id, "cancelled before dispatch")?;
                    settled += 1;
                }
            }
            after = rows.last().map(|t| t.id.clone());
            if rows.len() < usize::from(MAX_PAGE) {
                break;
            }
        }
        Ok(settled)
    }

    fn execute_once(&self, operation: &OperationRecord) -> EffectOutcome {
        let job = match self.job(operation) {
            Ok(j) => j,
            Err(e) => return e,
        };
        let (ctx, repository) = match self.execution_target(&job, &operation.id) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let stop = || self.git_stopped(&job);
        let helpers = crate::services::git::helper::RecordedHelpers {
            inner: &*self.helpers,
            operation: &operation.id,
        };
        let git = Git {
            runner: &helpers,
            ctx: &ctx,
            program: &self.config.program,
            deadline: self.config.deadline,
            stop: &stop,
        };
        let path = Path::new(&job.tree.path);
        if matches!(job.action, Action::Cleanup)
            && job.cleanup_proof.is_some()
            && self.cleanup_path_missing(path)
        {
            return self.resume_delivery_cleanup(&job, operation, &git, &repository);
        }
        if !matches!(job.action, Action::Create)
            && self
                .verify_target(
                    &git,
                    &Target {
                        repository: repository.clone(),
                        worktree: Some(job.tree.clone()),
                    },
                )
                .is_err()
        {
            return changed("physical repository or worktree identity changed");
        }
        let outcome =
            match &job.action {
                Action::Documentation(draft) => {
                    let evidence = draft.evidence.clone();
                    let actor = crate::services::git::context::bind_worktree(
                        &*self.store,
                        ctx.clone(),
                        &job.tree.id,
                    );
                    let Ok(actor) = actor else {
                        return denied("documentation worktree authority changed");
                    };
                    let valid = run_write(&*self.store, WriteOptions::host_state(), move |tx| {
                        evidence
                            .validate_in(tx, &actor)
                            .map_err(Failure::into_storage)
                    });
                    if !matches!(valid, Ok(true)) {
                        return changed("documentation source authority changed before generation");
                    }
                    let Some(source) = self
                        .documentation_analysis()
                        .and_then(|analysis| analysis.documentation_source())
                    else {
                        return denied("repository documentation source intake is unavailable");
                    };
                    let source = source.as_ref();
                    match documentation::apply(&git, source, path, &job, &operation.id, draft) {
                        Ok(()) => return self.observe_pending(&job, operation),
                        Err(GitFault::SourceChanged) => {
                            return changed("documentation source or target changed");
                        }
                        Err(_) => return EffectOutcome::Unknown(
                            "repository documentation could not be verified after the Git effect"
                                .into(),
                        ),
                    }
                }
                Action::Integrate => return self.execute_integrate(&job, operation, &ctx),
                Action::Create => {
                    if let Some(binding) = &job.reviewed_binding {
                        match self.approval_root_witness(&job.tree.registered_root_id) {
                            Ok(current) if binding["native"]["rootWitness"] == current => {}
                            _ => return changed("reviewed worktree root changed before creation"),
                        }
                    }
                    if self.root_path(&job.tree.registered_root_id).is_err() {
                        return denied("registered root changed");
                    }
                    let destination = match crate::services::git::client::path_argument(path) {
                        Ok(value) => value,
                        Err(_) => {
                            return failed(
                                DomainErrorCode::UnsupportedTarget,
                                "Git worktree path is unsupported",
                            );
                        }
                    };
                    match git.worktrees(Path::new(&repository.location)) {
                        Ok(trees)
                            if trees
                                .iter()
                                .any(|t| self.same_worktree_path(Path::new(&t.path), path)) =>
                        {
                            return denied("worktree path is already registered");
                        }
                        Err(_) => return denied("repository worktrees cannot be verified"),
                        _ => {}
                    }
                    let args = [
                        "worktree",
                        "add",
                        "-b",
                        job.tree
                            .branch
                            .strip_prefix("refs/heads/")
                            .expect("validated branch"),
                        "--",
                        &destination,
                        &job.tree.head.hex,
                    ];
                    if let Some(binding) = &job.reviewed_binding {
                        // worktree add -b internally launches branch/reset children. Execute
                        // those exact builtins separately so every step keeps the child ban.
                        (|| -> Result<Vec<u8>, GitFault> {
                            let repository_path = Path::new(&repository.location);
                            let branch = job
                                .tree
                                .branch
                                .strip_prefix("refs/heads/")
                                .expect("validated branch");
                            let policy = &binding["native"]["executionPolicy"];
                            git.reviewed_mutation(
                                repository_path,
                                &["branch", "--", branch, &job.tree.head.hex],
                                None,
                                &operation.id,
                                policy,
                            )?;
                            git.reviewed_mutation(
                                repository_path,
                                &[
                                    "worktree",
                                    "add",
                                    "--no-checkout",
                                    "--",
                                    &destination,
                                    branch,
                                ],
                                None,
                                &operation.id,
                                policy,
                            )?;
                            let location = self.resolve_location(&git, &repository, &job.tree)?;
                            if self
                                .approval_root_witness(&job.tree.registered_root_id)
                                .ok()
                                .as_ref()
                                != Some(&binding["native"]["rootWitness"])
                                || git.head(path)?.as_deref() != Some(job.tree.head.hex.as_str())
                                || git.branch(path)?.as_deref() != Some(job.tree.branch.as_str())
                            {
                                return Err(GitFault::SourceChanged);
                            }
                            // Conditional config can legitimately differ in the newly created
                            // Git directory. Preserve the qualified binary identity and bind the
                            // current target hook policy; no hook acquires execution authority.
                            let target_policy = git.reviewed_commit_policy(path)?;
                            if target_policy["gitProgram"] != policy["gitProgram"]
                                || target_policy["gitVersion"] != policy["gitVersion"]
                            {
                                return Err(GitFault::SourceChanged);
                            }
                            let result = git.reviewed_mutation(
                                path,
                                &["checkout", "--no-recurse-submodules", branch],
                                None,
                                &operation.id,
                                &target_policy,
                            )?;
                            if self.resolve_location(&git, &repository, &job.tree)? != location {
                                return Err(GitFault::SourceChanged);
                            }
                            Ok(result)
                        })()
                    } else {
                        git.mutation(
                            Path::new(&repository.location),
                            &args,
                            None,
                            &operation.id,
                            false,
                        )
                    }
                }
                Action::Inspect => Ok(vec![]),
                Action::Cleanup => return self.execute_cleanup(&job, operation, &git, &repository),
                Action::PullRequest { .. } => {
                    return self.execute_pull_request(&job, operation, &git, path);
                }
                Action::Push { .. } => return self.execute_push(&job, operation, &git, path),
                Action::Commit { message, expected } => {
                    if let Some(binding) = &job.reviewed_binding {
                        match git.commit_manifest(path, &expected.hex) {
                            Ok(manifest)
                                if super::approval::compact_commit_manifest(&manifest)
                                    .ok()
                                    .as_ref()
                                    == Some(&binding["commitManifest"]) => {}
                            _ => return changed("reviewed commit contents changed before staging"),
                        }
                    }
                    match if job.reviewed_binding.is_some() {
                        git.reviewed_revision_observation(path)
                    } else {
                        git.revision_observation(path)
                    } {
                        Ok(o)
                            if o.head == expected.hex
                                && o.branch.as_deref() == Some(&job.tree.branch) =>
                        {
                            if !o.dirty {
                                let docs = match documentation::matching_commit(
                                    &*self.store,
                                    &git,
                                    path,
                                    &job,
                                    &o.head,
                                ) {
                                    Ok(docs) => docs,
                                    Err(error) => return storage_failure(error.into_storage()),
                                };
                                return self.finish_git(&job, &operation.id, o, docs);
                            }
                        }
                        _ => return changed("worktree head or branch changed"),
                    }
                    let mutate = |args: &[&str], stdin| {
                        if let Some(binding) = &job.reviewed_binding {
                            git.reviewed_mutation(
                                path,
                                args,
                                stdin,
                                &operation.id,
                                &binding["commitManifest"]["executionPolicy"],
                            )
                        } else {
                            git.mutation(path, args, stdin, &operation.id, false)
                        }
                    };
                    match mutate(&["add", "--all", "--"], None) {
                        Ok(_) => {
                            if let Some(binding) = &job.reviewed_binding {
                                match git.staged_commit_manifest(path, &expected.hex) {
                                    Ok(manifest)
                                        if super::approval::compact_commit_manifest(&manifest)
                                            .ok()
                                            .as_ref()
                                            == Some(&binding["commitManifest"]) => {}
                                    _ => {
                                        return changed(
                                            "staged commit differs from the reviewed contents",
                                        );
                                    }
                                }
                            }
                            let mut args = vec!["commit", "--cleanup=verbatim", "--file=-"];
                            if job.reviewed_binding.is_some() {
                                args.push("--no-gpg-sign");
                            }
                            mutate(&args, Some(message.as_bytes().to_vec()))
                        }
                        Err(e) => Err(e),
                    }
                }
            };
        if outcome.is_err() {
            return EffectOutcome::Unknown(
                "Git helper did not establish the requested postcondition".into(),
            );
        }
        match if job.reviewed_binding.is_some() {
            git.reviewed_revision_observation(path)
        } else {
            git.revision_observation(path)
        } {
            Ok(observation) => {
                if let Action::Commit { expected, .. } = &job.action
                    && (observation.head == expected.hex
                        || git.commit_parents(path, &observation.head).ok().as_deref()
                            != Some(std::slice::from_ref(&expected.hex)))
                {
                    return EffectOutcome::Unknown("commit parent could not be verified".into());
                }
                if matches!(job.action, Action::Commit { .. })
                    && job.reviewed_binding.is_some()
                    && !reviewed_commit_matches(&git, path, &job, &observation.head)
                {
                    return EffectOutcome::Unknown(
                        "committed tree or message differs from the reviewed contents".into(),
                    );
                }
                let docs = if matches!(job.action, Action::Commit { .. }) {
                    match documentation::matching_commit(
                        &*self.store,
                        &git,
                        path,
                        &job,
                        &observation.head,
                    ) {
                        Ok(value) => value,
                        Err(error) => return storage_failure(error.into_storage()),
                    }
                } else {
                    None
                };
                self.finish_git(&job, &operation.id, observation, docs)
            }
            Err(_) => EffectOutcome::Unknown("Git result could not be observed".into()),
        }
    }

    fn observe_pending(&self, job: &Job, operation: &OperationRecord) -> EffectOutcome {
        let Some(ctx) = job.context(self, &operation.id) else {
            return denied("invalid principal kind");
        };
        self.observe_transition(job, operation, &ctx, true)
    }

    /// Explicit restore inspection of an archived local Git effect. The saved payload does
    /// not authorize a helper, and the active profile must prove the same current physical
    /// worktree before any readback. Foreign or network-owned effects remain held.
    pub(crate) fn observe_archived_local(
        &self,
        ctx: &InvocationContext,
        operation: &OperationRecord,
    ) -> Option<Value> {
        if ctx.principal_kind() != PrincipalKind::Human
            || !matches!(operation.capability.as_str(), "git.commit" | "git.worktree")
        {
            return None;
        }
        let job = self.job(operation).ok()?;
        match (operation.capability.as_str(), &job.action) {
            ("git.commit", Action::Commit { expected, .. }) => {
                crate::protocol::Check::check(expected).ok()?;
            }
            ("git.worktree", Action::Create) => {}
            _ => return None,
        }
        let current = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            let tree = read.git().git_worktree(&job.tree.id)?;
            let repository = read.projects().repository(&job.tree.repository_id)?;
            Ok(tree.zip(repository))
        })
        .ok()
        .flatten()?;
        let (tree, repository) = current;
        if tree.path != job.tree.path
            || tree.branch != job.tree.branch
            || tree.repository_id != job.tree.repository_id
            || tree.location.is_none()
            || tree.location != job.tree.location
            || tree.registered_root_id != job.tree.registered_root_id
        {
            return None;
        }
        let ctx = ctx
            .clone()
            .with_lineage(Some(job.project.clone()), None, None);
        let git = Git {
            runner: &*self.helpers,
            ctx: &ctx,
            program: &self.config.program,
            deadline: self.config.deadline,
            stop: &|| false,
        };
        let target = Target {
            repository,
            worktree: Some(tree),
        };
        self.verify_target(&git, &target).ok()?;
        let EffectOutcome::Completed(evidence) =
            self.observe_transition(&job, operation, &ctx, false)
        else {
            return None;
        };
        self.verify_target(&git, &target).ok()?;
        Some(evidence)
    }

    fn observe_transition(
        &self,
        job: &Job,
        operation: &OperationRecord,
        ctx: &InvocationContext,
        settle: bool,
    ) -> EffectOutcome {
        let stop = || false; // observation is permitted while the originating work is paused
        let helpers = crate::services::git::helper::RecordedHelpers {
            inner: &*self.helpers,
            operation: &operation.id,
        };
        let git = Git {
            runner: if settle { &helpers } else { &*self.helpers },
            ctx,
            program: &self.config.program,
            deadline: self.config.deadline,
            stop: &stop,
        };
        let path = Path::new(&job.tree.path);
        if settle && matches!(job.action, Action::Cleanup) {
            let repository = read_snapshot(&*self.store, &ReadOptions::default(), |s| {
                s.projects().repository(&job.tree.repository_id)
            });
            return match repository {
                Ok(Some(repository)) => self.observe_cleanup(job, operation, &git, &repository),
                _ => EffectOutcome::Unknown("cleanup repository is unavailable".into()),
            };
        }
        if settle && !matches!(job.action, Action::Create) {
            let repository = read_snapshot(&*self.store, &ReadOptions::default(), |s| {
                s.projects().repository(&job.tree.repository_id)
            });
            if !matches!(
                repository,
                Ok(Some(repository))
                    if self
                        .verify_target(
                            &git,
                            &Target {
                                repository: repository.clone(),
                                worktree: Some(job.tree.clone()),
                            },
                        )
                        .is_ok()
            ) {
                return EffectOutcome::Unknown(
                    "physical Git identity cannot be verified for recovery".into(),
                );
            }
        }
        if let Action::PullRequest {
            repository,
            head,
            final_feature,
            ..
        } = &job.action
        {
            let Some(client) = &self.config.github else {
                return EffectOutcome::Unknown("GitHub observation is unavailable".into());
            };
            let observed = if *final_feature {
                client.find_feature(&*self.store, repository, head)
            } else {
                client.find(&*self.store, repository, head)
            };
            return match observed {
                Ok(Some(pr)) => self.finish_pull_request(job, &operation.id, pr),
                _ => EffectOutcome::Unknown(
                    "PR outcome has not been established; no write was repeated".into(),
                ),
            };
        }
        let observed = match if job.reviewed_binding.is_some() {
            git.reviewed_revision_observation(path)
        } else {
            git.revision_observation(path)
        } {
            Ok(o) if o.branch.as_deref() == Some(&job.tree.branch) => o,
            _ => return EffectOutcome::Unknown("worktree outcome is not observable".into()),
        };
        let verified = match &job.action {
            Action::Documentation(draft) => {
                observed.head == job.tree.head.hex
                    && self
                        .documentation_analysis()
                        .and_then(|analysis| analysis.documentation_source())
                        .is_some_and(|source| {
                            documentation::observed(&git, source.as_ref(), path, draft)
                        })
            }
            Action::Create => {
                let repository = read_snapshot(&*self.store, &ReadOptions::default(), |s| {
                    s.projects().repository(&job.tree.repository_id)
                });
                observed.head == job.tree.head.hex
                    && matches!(
                        repository,
                        Ok(Some(repository))
                            if git.worktrees(Path::new(&repository.location)).is_ok_and(
                                |trees| {
                                    trees.iter().any(|tree| {
                                        self.same_worktree_path(Path::new(&tree.path), path)
                                            && tree.branch.as_deref() == Some(&job.tree.branch)
                                    })
                                },
                            )
                    )
            }
            Action::Inspect => true,
            Action::Cleanup => unreachable!("handled above"),
            Action::Commit { message, expected } => {
                if observed.head == expected.hex {
                    settle && !observed.dirty && job.reviewed_binding.is_none()
                } else {
                    git.commit_parents(path, &observed.head).ok().as_deref()
                        == Some(std::slice::from_ref(&expected.hex))
                        && if job.reviewed_binding.is_some() {
                            reviewed_commit_matches(&git, path, job, &observed.head)
                        } else {
                            git.commit_message(path, &observed.head)
                                .is_ok_and(|body| body.trim_end() == message.trim_end())
                        }
                }
            }
            Action::Push {
                url,
                target,
                expected,
                transport_digest,
                task_publication,
                ..
            } => {
                let transport = match self.push_transport(
                    &git,
                    path,
                    &expected.format,
                    &job.tree.repository_id,
                ) {
                    Ok(transport) if transport_digest.as_ref() == Some(&transport.digest) => {
                        transport
                    }
                    _ => {
                        return EffectOutcome::Unknown(
                            "trusted push transport cannot be established for recovery".into(),
                        );
                    }
                };
                let network = Git {
                    runner: &transport,
                    ctx: git.ctx,
                    program: git.program,
                    deadline: git.deadline,
                    stop: git.stop,
                };
                observed.head == expected.hex
                    && matches!(network.remote_symbolic_target(path, url, target), Ok(None))
                    && network
                        .remote_head(path, url, target)
                        .ok()
                        .flatten()
                        .is_some_and(|remote| {
                            remote == expected.hex
                                || (task_publication.is_some()
                                    && git.is_ancestor(path, &expected.hex, &remote) == Ok(true))
                        })
            }
            Action::PullRequest { .. } | Action::Integrate => unreachable!("handled above"),
        };
        if verified && settle {
            let docs = if matches!(job.action, Action::Commit { .. } | Action::Push { .. }) {
                match documentation::matching_commit(&*self.store, &git, path, job, &observed.head)
                {
                    Ok(value) => value,
                    Err(error) => return storage_failure(error.into_storage()),
                }
            } else {
                None
            };
            self.finish_git(job, &operation.id, observed, docs)
        } else if verified {
            EffectOutcome::Completed(json!({ "kind":"VerifiedGitWorktree", "head":observed.head,
                "branch":observed.branch }))
        } else {
            EffectOutcome::Unknown(
                "Git postcondition has not been established; no write was repeated".into(),
            )
        }
    }

    /// Recovery never repeats a write. A completed local checkpoint is sufficient proof;
    /// otherwise preserve the reservation for explicit reconciliation.
    pub fn observe(&self, operation: &OperationRecord) -> EffectOutcome {
        let job = match self.job(operation) {
            Ok(j) => j,
            Err(e) => return e,
        };
        let current = match read_snapshot(&*self.store, &ReadOptions::default(), |s| {
            s.git().git_worktree(&job.tree.id)
        }) {
            Ok(Some(t)) => t,
            Ok(None) => {
                return EffectOutcome::Unknown("Git ownership checkpoint is missing".into());
            }
            Err(e) => return storage_failure(e),
        };
        if current.pending_operation_id.as_ref() == Some(&operation.id) && current == job.tree {
            if matches!(job.action, Action::Integrate) {
                let outcome = self.observe_integrate(&job, operation);
                if let EffectOutcome::Failed(reason) = &outcome
                    && let Err(error) = self.settle_denial(&job, &operation.id, reason)
                {
                    return storage_failure(error);
                }
                return outcome;
            }
            return self.observe_pending(&job, operation);
        }
        if current.last_operation_id.as_ref() != Some(&operation.id) {
            return EffectOutcome::Unknown(
                "Git effect has no verified completion checkpoint; it was not replayed".into(),
            );
        }
        match current.last_result {
            Some(result) if result.get("result").is_some() => EffectOutcome::Completed(result),
            Some(result) if result.get("failed").and_then(Value::as_str).is_some() => {
                EffectOutcome::Failed(result["failed"].as_str().expect("checked").into())
            }
            _ => EffectOutcome::Unknown("Git completion checkpoint is missing".into()),
        }
    }
}

impl CommandHandler for GitService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        if let OperationInput::RepositoryDocs(
            crate::operations::repository_docs::Input::Generate(input),
        ) = command.input()
        {
            return self.prepare_repository_docs(ctx, input, env, None);
        }
        if let OperationInput::Git(ops::Input::PullRequest(input)) = command.input() {
            return self.prepare_pull_request(ctx, &input.command, env);
        }
        if let OperationInput::Git(ops::Input::Push(input)) = command.input() {
            return self.prepare_push(ctx, &input.command, env);
        }
        let OperationInput::Git(ops::Input::Commit(input)) = command.input() else {
            return Ok(None);
        };
        let GitCommand::Commit { message, .. } = &input.command else {
            return Ok(None);
        };
        if env.stopped(self.clock.now_ms().get()) {
            return Err(StorageError::Cancelled);
        }
        let text = match self.commit_message_text(ctx, message) {
            Ok(text) => Ok(text),
            Err(Failure::Refused(r)) => Err(r.code),
            Err(Failure::Storage(e)) => return Err(e),
        };
        let manifest = if ctx.reviewed_operation().is_some() {
            match self.commit_source_manifest(ctx, &input.command, env) {
                Ok(manifest) => Some(match super::approval::compact_commit_manifest(&manifest) {
                    Ok(value) => value,
                    Err(Failure::Refused(r)) => {
                        return Ok(Some(Prepared::new(StagedMessage(Err(r.code), None))));
                    }
                    Err(Failure::Storage(e)) => return Err(e),
                }),
                Err(Failure::Refused(r)) => {
                    return Ok(Some(Prepared::new(StagedMessage(Err(r.code), None))));
                }
                Err(Failure::Storage(e)) => return Err(e),
            }
        } else {
            None
        };
        Ok(Some(Prepared::new(StagedMessage(text, manifest))))
    }
    fn settle(
        &self,
        _ctx: &InvocationContext,
        prepared: Option<&Prepared>,
        reply: Option<&crate::protocol::results::DomainCommandReply>,
        _env: &Preparation<'_>,
    ) {
        let Some(staged) = prepared
            .and_then(|p| p.get::<push::StagedPush>())
            .and_then(push::StagedPush::integration_diff)
        else {
            return;
        };
        let committed = matches!(reply, Some(crate::protocol::CommandReply::Recorded { receipt, replayed: false, .. })
            if matches!(receipt.decision, DomainDecision::Accepted {..}));
        self.artifacts
            .settle_host_bytes(self.store.as_ref(), staged, committed);
    }
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let result = self.decide_git(ctx, command, tx);
        match result {
            Ok(d) => Ok(d),
            Err(f) => decided(tx, Err(f)),
        }
    }
}

impl JournaledEffect for GitService {
    fn resumes_after_observation(&self, operation: &OperationRecord) -> bool {
        self.job(operation).is_ok_and(|job| match job.action {
            Action::Push { .. } => job.tree.branch.starts_with("refs/heads/feature/"),
            Action::PullRequest { final_feature, .. } => final_feature,
            Action::Integrate => true,
            Action::Cleanup => job.cleanup_proof.is_some(),
            _ => false,
        })
    }

    fn execute(&self, operation: &OperationRecord) -> EffectOutcome {
        GitService::execute(self, operation)
    }
    fn observe(&self, operation: &OperationRecord) -> EffectOutcome {
        GitService::observe(self, operation)
    }
}
