//! Journaled writes of the Git owner. A worktree reservation serializes writes; an unknown
//! result keeps the reservation until observation proves the requested postcondition.
mod attach;
mod pull_request;
mod push;
use super::*;
use crate::authority::destructive::EntryKind;
use crate::context::PrincipalKind;
use crate::gateway::AdmittedCommand;
use crate::ports::storage::*;
use crate::ports::{AuthorityFence, CommandHandler, Decided, HandlerFailure};
use crate::protocol::data::GitCommand;
use crate::protocol::records::GitActionKind;
use crate::protocol::results::DomainDecision;
use crate::protocol::{ActorId, AssignmentId, EventId, OperationId, ProjectId, RootId};
use crate::services::sessions::{counter, decided};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Read;
use std::path::PathBuf;

pub const EFFECT_CONSUMER: &str = "git_effects";

/// Host-registered root for newly created worktrees. A tool selects only the opaque ID.
#[derive(Clone)]
pub struct RegisteredGitRoot {
    pub id: Id,
    pub path: PathBuf,
}
#[derive(Debug, PartialEq)]
pub enum EffectOutcome {
    Completed(Value),
    Failed(String),
    Unknown(String),
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
}
#[derive(Clone, Serialize, Deserialize)]
enum Action {
    PullRequest {
        repository: super::github::Repository,
        head: String,
        base: String,
        title: String,
        description: String,
        expected: GitOid,
        covered: Vec<(Id, i64)>,
    },
    Create,
    Inspect,
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
    fn context(&self, service: &GitService, operation: &OperationId) -> InvocationContext {
        let kind = match self.kind.as_str() {
            "Human" => PrincipalKind::Human,
            "Agent" => PrincipalKind::Agent,
            "Package" => PrincipalKind::Package,
            "McpClient" => PrincipalKind::McpClient,
            "Schedule" => PrincipalKind::Schedule,
            _ => PrincipalKind::Host,
        };
        let mut ctx = InvocationContext::new(
            self.actor.clone(),
            kind,
            operation.0.clone(),
            Id::new("git-effects").expect("namespace"),
            service.store.info().store_id,
            counter(self.restore_epoch),
            rev(1),
        )
        .with_lineage(
            Some(self.project.clone()),
            self.root.clone(),
            self.assignment.clone(),
        );
        if let Some(session) = &self.session {
            ctx = ctx.with_session(session.clone());
        }
        ctx
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
        crate::services::indexing::git::path_argument(&root.path)
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
        if let Some(feature) = &work.parent_feature_id
            && !tx
                .git()
                .git_worktrees(feature, None, MAX_PAGE)?
                .iter()
                .any(|t| {
                    t.repository_id == repository.id
                        && !t.removed
                        && t.pending_operation_id.is_none()
                        && t.head == input.base
                })
        {
            return Err(refuse(DomainErrorCode::Conflict, "feature_branch", feature));
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
            let control = self
                .approval_control
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            crate::services::approvals::owner_preview::recheck_control(
                tx,
                ctx,
                control.as_deref(),
            )?;
            self.validate_reviewed_git_command(tx, ctx, command, binding)?;
        }
        let op = OperationId(tx.new_id(IdKind::Operation));
        let (mut tree, repository, work, action) = match command.input() {
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
                        let role = if work.kind == FEATURE {
                            "feature"
                        } else {
                            "task"
                        };
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
                            branch: format!("refs/heads/devworks/{role}-{work_component}"),
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
            feature_id: work.parent_feature_id.clone(),
            actor: ctx.principal().clone(),
            kind: format!("{:?}", ctx.principal_kind()),
            session: ctx.session_id().cloned(),
            project: work.project_id.clone(),
            root: ctx.root_id().cloned(),
            assignment: ctx.assignment_id().cloned(),
            restore_epoch: ctx.restore_epoch().get(),
            repository_revision: repository.revision,
            work_revision: work.revision,
            root_epochs,
            assignment_generation,
            fences,
            reviewed_binding,
            tree: tree.clone(),
            action,
        };
        tx.operations().record_intent(NewOperation {
            id: op.clone(),
            root_id: job.root.clone(),
            assignment_id: job.assignment.clone(),
            capability: command.descriptor().name.into(),
            schema_version: 1,
            args_digest: command.fingerprint().as_str().into(),
            payload: serde_json::to_value(job)
                .map_err(|e| StorageError::InvalidRecord(e.to_string()))?,
        })?;
        if let Some(previous_owner) = previous_owner {
            let event = EventId(tx.new_id(IdKind::Event));
            tx.history().append(NewEvent { id: event, actor: ctx.principal().clone(), project_id: Some(work.project_id.clone()), root_id: Some(RootId(work.id.clone())), operation_id: Some(op.clone()), aggregate_kind: "git_ownership".into(), aggregate_id: tree.id.as_str().into(), aggregate_revision: tree.ownership_revision, kind: "GitWorktreeOwnershipTransferred".into(), payload: json!({"repositoryId":tree.repository_id,"workId":tree.work_id,"worktreeId":tree.id,"previousOwner":previous_owner,"owner":tree.owner,"ownershipRevision":tree.ownership_revision,"path":tree.path,"branch":tree.branch}) })?;
        }
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent { id: event.clone(), actor: ctx.principal().clone(), project_id: Some(work.project_id), root_id: ctx.root_id().cloned(), operation_id: Some(op.clone()), aggregate_kind: "git_worktree".into(), aggregate_id: tree.id.as_str().into(), aggregate_revision: tree.revision, kind: "GitWriteRequested".into(), payload: json!({"repositoryId":tree.repository_id,"workId":tree.work_id,"worktreeId":tree.id,"branch":tree.branch,"expectedHead":tree.head}) })?;

        let now = tx.now_ms();
        tx.queue().enqueue(NewQueueEntry {
            id: queue,
            consumer: EFFECT_CONSUMER.into(),
            event_id: event,
            root_id: ctx.root_id().cloned(),
            operation_id: Some(op.clone()),
            partition: Some(tree.id.as_str().into()),
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
        let ctx = job.context(self, operation);
        let j = job.clone();
        let c = ctx.clone();
        let repository = run_write(&*self.store, WriteOptions::default(), move |tx| {
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
            // Domain scope checks below run on the same current snapshot as the reservation.
            if work.revision != j.work_revision || repository.revision != j.repository_revision {
                return Err(StorageError::ScopeDenied);
            }
            let brief = crate::services::work::model::WorkBrief::of(&work)?;
            let managing =
                matches!(j.action, Action::PullRequest { .. }) && Self::pr_manager(tx, &c, &work)?;
            if let Action::PullRequest { covered, .. } = &j.action {
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
        observation: crate::services::indexing::git::RevisionObservation,
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
        let ctx = job.context(self, operation);
        let stop = || false;
        let helpers = crate::services::indexing::helper::RecordedHelpers {
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
            Action::Commit { .. } => OperationResult::GitCommit { value },
            Action::Push { .. } => OperationResult::GitPush { value },
            Action::PullRequest { .. } => {
                return EffectOutcome::Unknown(
                    "PR completion requires a remote observation".into(),
                );
            }
            _ => OperationResult::GitWorktree { value },
        };
        let j = job.clone();
        let op = operation.clone();
        let checkpoint = json!({"result":result});
        let durable = checkpoint.clone();
        let outcome = run_write(&*self.store, WriteOptions::default(), move |tx| {
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
            let id = EventId(tx.new_id(IdKind::Event));
            let sequence = tx.history().append(NewEvent { id: id.clone(), actor: j.actor.clone(), project_id: Some(j.project.clone()), root_id: j.root.clone(), operation_id: Some(op.clone()), aggregate_kind: "git_worktree".into(), aggregate_id: updated.id.as_str().into(), aggregate_revision: updated.revision, kind: "GitWriteObserved".into(), payload: json!({"repositoryId":updated.repository_id,"workId":updated.work_id,"worktreeId":updated.id,"path":updated.path,"branch":updated.branch,"head":head}) })?;
            let mut roots = vec![RootId(updated.work_id.clone())];
            if let Some(feature) = &j.feature_id {
                roots.push(RootId(feature.clone()));
            }
            for root in roots {
                let link = tx.new_id(IdKind::Link);
                let now = tx.now_ms();
                tx.sessions().insert_link(&StoredLink { id: link, project_id: j.project.clone(), root_id: root, kind: "git".into(), target_kind: "git_worktree".into(), target_id: updated.id.as_str().into(), parent_id: None, supersedes_id: None, actor: j.actor.clone(), event_id: id.0.clone(), sequence, repository_id: Some(updated.repository_id.clone()), revision: Some(head.hex.clone()), summary: format!("{} at {}", updated.branch, updated.path), body: json!({"agentSessionId":j.session,"worktreeId":updated.id,"path":updated.path,"branch":updated.branch,"head":head}), created_at_ms: now })?;
            }
            Ok(())
        });
        match outcome {
            Ok(()) => EffectOutcome::Completed(checkpoint),
            Err(e) => storage_failure(e),
        }
    }

    fn git_stopped(&self, job: &Job) -> bool {
        if self.is_stopped() {
            return true;
        }
        read_snapshot(&*self.store, &ReadOptions::default(), |snapshot| {
            let authorization = crate::ports::Authorization::direct().fenced_by(job.fences.clone());
            if !crate::authority::authorization_holds(snapshot, &authorization)?
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
                    && (control.desired == DesiredExecution::Paused
                        || control.stop_operation_id.is_some()
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
        run_write(&*self.store, WriteOptions::default(), move |tx| {
            let Some(mut tree) = tx.git().git_worktree(&job.tree.id)? else {
                return Ok(());
            };
            if tree.pending_operation_id.as_ref() != Some(&operation) {
                return Ok(());
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
            tx.history().append(NewEvent { id: event, actor: job.actor.clone(), project_id: Some(job.project.clone()), root_id: job.root.clone(), operation_id: Some(operation.clone()), aggregate_kind: "git_worktree".into(), aggregate_id: tree.id.as_str().into(), aggregate_revision: tree.revision, kind: "GitWriteRefusedBeforeDispatch".into(), payload: json!({"repositoryId":tree.repository_id,"workId":tree.work_id,"reason":reason}) })?;
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
        let helpers = crate::services::indexing::helper::RecordedHelpers {
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
        let outcome = match &job.action {
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
                let destination = match crate::services::indexing::git::path_argument(path) {
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
                            return self.finish_git(&job, &operation.id, o);
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
                self.finish_git(&job, &operation.id, observation)
            }
            Err(_) => EffectOutcome::Unknown("Git result could not be observed".into()),
        }
    }

    fn observe_pending(&self, job: &Job, operation: &OperationRecord) -> EffectOutcome {
        let ctx = job.context(self, &operation.id);
        let stop = || false; // observation is permitted while the originating work is paused
        let helpers = crate::services::indexing::helper::RecordedHelpers {
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
        if !matches!(job.action, Action::Create) {
            let repository = read_snapshot(&*self.store, &ReadOptions::default(), |s| {
                s.projects().repository(&job.tree.repository_id)
            });
            if !matches!(repository, Ok(Some(repository)) if self.verify_target(&git, &Target { repository: repository.clone(), worktree: Some(job.tree.clone()) }).is_ok())
            {
                return EffectOutcome::Unknown(
                    "physical Git identity cannot be verified for recovery".into(),
                );
            }
        }
        if let Action::PullRequest {
            repository, head, ..
        } = &job.action
        {
            let Some(client) = &self.config.github else {
                return EffectOutcome::Unknown("GitHub observation is unavailable".into());
            };
            return match client.find(&*self.store, repository, head) {
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
            Action::Create => {
                let repository = read_snapshot(&*self.store, &ReadOptions::default(), |s| {
                    s.projects().repository(&job.tree.repository_id)
                });
                observed.head == job.tree.head.hex
                    && matches!(repository, Ok(Some(repository)) if git.worktrees(Path::new(&repository.location)).is_ok_and(|trees| trees.iter().any(|tree| self.same_worktree_path(Path::new(&tree.path), path) && tree.branch.as_deref() == Some(&job.tree.branch))))
            }
            Action::Inspect => true,
            Action::Commit { message, expected } => {
                if observed.head == expected.hex {
                    !observed.dirty && job.reviewed_binding.is_none()
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
                        .as_deref()
                        == Some(&expected.hex)
            }
            Action::PullRequest { .. } => unreachable!("handled above"),
        };
        if verified {
            self.finish_git(job, &operation.id, observed)
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
