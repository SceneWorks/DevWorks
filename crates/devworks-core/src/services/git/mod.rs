//! Native Git use cases. All process work is prepared outside store snapshots; the answer
//! rechecks repository/worktree ownership against the snapshot the gateway returns.
mod approval;
mod commands;
pub mod context;
pub mod github;
mod location;
mod transport;
use crate::context::InvocationContext;
use crate::gateway::AdmittedQuery;
use crate::operations::{OperationInput, git as ops};
use crate::ports::storage::ArtifactScope;
use crate::ports::storage::{ApplicationStore, ReadSnapshot, StoredGitWorktree, StoredRepository};
use crate::ports::{Clock, IdKind, IdSource, Preparation, Prepared, QueryAnswer, QueryHandler};
use crate::protocol::data::{GitOid, ObjectFormat};
use crate::protocol::records::GitInspection;
use crate::protocol::results::OperationResult;
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, Id, OperationState};
use crate::services::artifacts::evidence::{Evidence, EvidenceFailure};
use crate::services::artifacts::{ArtifactService, AuthorizedScope, authorized_scope};
use crate::services::indexing::git::{Git, GitFault};
use crate::services::indexing::helper::{HelperFault, HelperRunner};
use crate::services::sessions::{Failure, Refusal, query_fault, rev};
pub use commands::{EFFECT_CONSUMER, EffectOutcome, RegisteredGitRoot};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct GitConfig {
    pub transport: Option<Arc<dyn crate::ports::git_transport::GitTransportFiles>>,
    pub program: String,
    pub deadline: Duration,
    pub roots: Vec<RegisteredGitRoot>,
    pub github: Option<Arc<github::GitHubClient>>,
    pub filesystem: Option<Arc<dyn crate::authority::destructive::FilesystemResolver>>,
}
impl Default for GitConfig {
    fn default() -> Self {
        Self {
            transport: None,
            program: "git".into(),
            deadline: Duration::from_secs(30),
            roots: Vec::new(),
            github: None,
            filesystem: None,
        }
    }
}
pub struct GitService {
    store: Arc<dyn ApplicationStore>,
    artifacts: Arc<ArtifactService>,
    helpers: Arc<dyn HelperRunner>,
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
    config: GitConfig,
    stopping: std::sync::atomic::AtomicBool,
    approval_control: std::sync::RwLock<Option<Arc<crate::services::runs::ControlService>>>,
}
#[derive(Clone)]
struct Target {
    repository: StoredRepository,
    worktree: Option<StoredGitWorktree>,
}
impl Target {
    fn path(&self) -> &Path {
        Path::new(
            self.worktree
                .as_ref()
                .map_or(self.repository.location.as_str(), |w| w.path.as_str()),
        )
    }
}
struct PreparedRead {
    target: Target,
    result: OperationResult,
}
struct Inspected {
    target: Target,
    value: GitInspection,
}
fn refuse(code: DomainErrorCode, kind: &'static str, id: &Id) -> Failure {
    Refusal::new(code, kind, Some(id.clone())).into()
}
fn read_failure(error: GitFault, repository: &Id) -> Failure {
    let (code, kind) = match error {
        GitFault::SourceChanged => (DomainErrorCode::SourceChanged, "git_source"),
        GitFault::Unqualified => (
            DomainErrorCode::UnsupportedCapability,
            "git_dirty_qualification",
        ),
        GitFault::ReviewedUnavailable => (
            DomainErrorCode::UnsupportedCapability,
            "git_reviewed_process_policy",
        ),
        GitFault::Unborn => (DomainErrorCode::UnsupportedCapability, "git_unborn"),
        GitFault::Malformed(_) => (DomainErrorCode::ExternalFailure, "git_output"),
        GitFault::Helper(HelperFault::Cancelled) => (DomainErrorCode::Cancelled, "git_read"),
        GitFault::Helper(HelperFault::TimedOut) => {
            (DomainErrorCode::ExternalFailure, "git_deadline")
        }
        GitFault::Helper(HelperFault::OutputTooLarge) => {
            (DomainErrorCode::InvalidArgument, "git_output_limit")
        }
        GitFault::Helper(HelperFault::Refused(_)) => {
            (DomainErrorCode::PermissionDenied, "git_launch")
        }
        GitFault::Helper(HelperFault::Spawn(_)) => {
            (DomainErrorCode::UnsupportedTarget, "git_executable")
        }
        GitFault::Helper(HelperFault::Uncertain(_)) => {
            (DomainErrorCode::ExternalFailure, "git_stop_unverified")
        }
        GitFault::Helper(HelperFault::Storage(_)) => {
            (DomainErrorCode::StorageUnavailable, "git_storage")
        }
        GitFault::Helper(HelperFault::Failed { .. }) => {
            (DomainErrorCode::ExternalFailure, "git_command")
        }
    };
    refuse(code, kind, repository)
}

impl GitService {
    pub fn new(
        store: Arc<dyn ApplicationStore>,
        helpers: Arc<dyn HelperRunner>,
        artifacts: Arc<ArtifactService>,
        ids: Arc<dyn IdSource>,
        clock: Arc<dyn Clock>,
        config: GitConfig,
    ) -> Self {
        Self {
            store,
            artifacts,
            helpers,
            ids,
            clock,
            config,
            stopping: std::sync::atomic::AtomicBool::new(false),
            approval_control: std::sync::RwLock::new(None),
        }
    }
    pub(crate) fn set_approval_control(&self, control: Arc<crate::services::runs::ControlService>) {
        *self
            .approval_control
            .write()
            .unwrap_or_else(|p| p.into_inner()) = Some(control);
    }
    pub fn stop(&self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn is_stopped(&self) -> bool {
        self.stopping.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Probe the composed native executable without opening a repository or the network.
    pub fn qualify(&self) -> bool {
        let Some(root) = self.config.roots.first() else {
            return false;
        };
        let info = self.store.info();
        let ctx = crate::context::HostIngress::new(&*self.store).context(
            info.host_actor,
            crate::context::PrincipalKind::Host,
            self.ids.next_id(IdKind::Command),
            Id::new("git-qualification").expect("namespace"),
            crate::services::sessions::counter(info.restore_epoch),
            rev(1),
        );
        let stop = || self.is_stopped();
        let git = Git {
            runner: &*self.helpers,
            ctx: &ctx,
            program: &self.config.program,
            deadline: self.config.deadline,
            stop: &stop,
        };
        git.version(&root.path).is_ok_and(|version| {
            let Some(number) = version.strip_prefix("git version ").and_then(|v| v.split_whitespace().next()) else { return false };
            let mut parts = number.split('.');
            matches!((parts.next().and_then(|v| v.parse::<u64>().ok()), parts.next().and_then(|v| v.parse::<u64>().ok())), (Some(major),Some(minor)) if (major,minor) >= (2,31))
        })
    }

    fn correlation(&self) -> CorrelationId {
        CorrelationId(self.ids.next_id(IdKind::Correlation))
    }
    fn target(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        repository: &Id,
        worktree: Option<&Id>,
    ) -> Result<Target, Failure> {
        let repo = snapshot
            .projects()
            .repository(repository)?
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, "repository", repository))?;
        let projects = snapshot.projects().repository_projects(repository)?;
        let bound = authorized_scope(snapshot.authority(), ctx)?;
        if ctx.project_id().is_some_and(|p| !projects.contains(p))
            || matches!(&bound,AuthorizedScope::Project(p) if !projects.contains(p))
        {
            return Err(refuse(DomainErrorCode::NotFound, "repository", repository));
        }
        if repo.lifecycle != "Active" {
            return Err(refuse(DomainErrorCode::Conflict, "repository", repository));
        }
        let tree = match worktree {
            None => None,
            Some(id) => {
                let tree = snapshot
                    .git()
                    .git_worktree(id)?
                    .ok_or_else(|| refuse(DomainErrorCode::NotFound, "worktree", id))?;
                let work = snapshot
                    .work()
                    .work(&tree.work_id)?
                    .ok_or_else(|| refuse(DomainErrorCode::NotFound, "worktree", id))?;
                if tree.repository_id != *repository
                    || tree.removed
                    || !projects.contains(&work.project_id)
                    || ctx.project_id().is_some_and(|p| p != &work.project_id)
                    || matches!(&bound,AuthorizedScope::Project(p) if p!=&work.project_id)
                    || !snapshot
                        .work()
                        .work_repositories(&work.id)?
                        .contains(repository)
                {
                    return Err(refuse(DomainErrorCode::NotFound, "worktree", id));
                }
                Some(tree)
            }
        };
        Ok(Target {
            repository: repo,
            worktree: tree,
        })
    }
    fn inspect(
        &self,
        ctx: &InvocationContext,
        target: Target,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Inspected, Failure> {
        let stopped = || env.stopped(self.clock.now_ms().get());
        let git = Git {
            runner: &*self.helpers,
            ctx,
            program: &self.config.program,
            deadline: self.config.deadline,
            stop: &stopped,
        };
        self.verify_target(&git, &target)
            .map_err(|e| read_failure(e, &target.repository.id))?;
        let observation = git
            .revision_observation(target.path())
            .map_err(|e| read_failure(e, &target.repository.id))?;
        let head = GitOid {
            format: if observation.head.len() == 64 {
                ObjectFormat::Sha256
            } else {
                ObjectFormat::Sha1
            },
            hex: observation.head,
        };
        let mut value = GitInspection {
            repository_id: target.repository.id.clone(),
            worktree_id: target.worktree.as_ref().map(|w| w.id.clone()),
            head,
            branch: observation
                .branch
                .map(crate::protocol::support::BoundedText),
            dirty: observation.dirty,
            state: OperationState::ObservedComplete,
            evidence: crate::protocol::support::BoundedList(vec![]),
        };
        let project = match (ctx.project_id(), target.worktree.as_ref()) {
            (Some(project), _) => Some(project.clone()),
            (None, Some(tree)) => env
                .read(|s| s.work().work(&tree.work_id))?
                .map(|w| w.project_id),
            _ => None,
        };
        let metadata = serde_json::json!({"schemaVersion":1,"kind":"GitInspectionMetadata","repositoryId":target.repository.id,"repositoryName":target.repository.name,"primaryCheckout":target.repository.location,"worktree":target.worktree.as_ref().map(|tree| serde_json::json!({"id":tree.id,"workId":tree.work_id,"path":tree.path,"branch":tree.branch,"ownerActorId":tree.owner,"ownershipRevision":tree.ownership_revision,"removed":tree.removed,"pendingOperationId":tree.pending_operation_id})),"registeredRoots":self.config.roots.iter().map(|root| serde_json::json!({"id":root.id,"path":root.path})).collect::<Vec<_>>(),"head":value.head,"branch":value.branch,"dirty":value.dirty});
        let bytes = serde_json::to_vec_pretty(&metadata)
            .map_err(|e| crate::ports::storage::StorageError::InvalidRecord(e.to_string()))?;
        let evidence = Evidence {
            scope: project.map_or(ArtifactScope::Workspace, ArtifactScope::Project),
            producer: ctx.principal().clone(),
            principal_kind: format!("{:?}", ctx.principal_kind()),
            agent_session: ctx.session_id().cloned(),
            root: ctx.root_id().cloned(),
            assignment: ctx.assignment_id().cloned(),
            title: "Git repository and worktree metadata".into(),
            media_type: "application/json".into(),
            tags: vec!["git".into(), "inspection".into()],
            source: serde_json::json!({"kind":"GitInspection","repositoryId":target.repository.id,"worktreeId":value.worktree_id,"head":value.head,"dirty":value.dirty}),
        };
        value
            .evidence
            .0
            .push(self.publish_read(ctx, &target, query, env, evidence, &bytes)?);
        Ok(Inspected { target, value })
    }
    fn publish_read(
        &self,
        ctx: &InvocationContext,
        target: &Target,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
        evidence: Evidence,
        bytes: &[u8],
    ) -> Result<crate::protocol::provider::ArtifactRef, Failure> {
        let auth = query.authorization().clone();
        let context = ctx.clone();
        let old = target.clone();
        let cancellation = env.cancellation.cloned();
        let deadline = env.deadline_ms;
        let (artifact, ()) = self
            .artifacts
            .publish_evidence(&*self.store, evidence, bytes, move |tx, _| {
                if cancellation.as_ref().is_some_and(|c| c.is_cancelled())
                    || deadline.is_some_and(|d| tx.now_ms() >= d)
                {
                    return Err(crate::ports::storage::StorageError::Cancelled);
                }
                if !crate::authority::authorization_holds_in(tx, &context, &auth)? {
                    return Err(crate::ports::storage::StorageError::ScopeDenied);
                }
                if let Some(project) = context.project_id()
                    && !tx
                        .projects()
                        .repository_projects(&old.repository.id)?
                        .contains(project)
                {
                    return Err(crate::ports::storage::StorageError::ScopeDenied);
                }
                let repo = tx.projects().repository(&old.repository.id)?;
                if repo.as_ref().map(|r| r.revision) != Some(old.repository.revision) {
                    return Err(crate::ports::storage::StorageError::ScopeDenied);
                }
                if let Some(tree) = &old.worktree {
                    if tx.git().git_worktree(&tree.id)?.as_ref() != Some(tree) {
                        return Err(crate::ports::storage::StorageError::ScopeDenied);
                    }
                    let work = tx
                        .work()
                        .work(&tree.work_id)?
                        .ok_or(crate::ports::storage::StorageError::ScopeDenied)?;
                    if !tx
                        .work()
                        .work_repositories(&work.id)?
                        .contains(&old.repository.id)
                        || !tx
                            .projects()
                            .repository_projects(&old.repository.id)?
                            .contains(&work.project_id)
                    {
                        return Err(crate::ports::storage::StorageError::ScopeDenied);
                    }
                }
                Ok(())
            })
            .map_err(|e| match e {
                EvidenceFailure::Refused(r) => {
                    Refusal::new(r.code, r.target_kind, r.target_id).into()
                }
                EvidenceFailure::Storage(e) => Failure::from(e),
            })?;
        Ok(artifact)
    }

    fn diff(
        &self,
        ctx: &InvocationContext,
        target: Target,
        input: &ops::DiffInput,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<PreparedRead, Failure> {
        if input.page.cursor.is_some() {
            return Err(crate::ports::storage::StorageError::CursorExpired.into());
        }
        if input.base.format != input.head.format {
            return Err(refuse(
                DomainErrorCode::InvalidArgument,
                "git_object_format",
                &target.repository.id,
            ));
        }
        let stopped = || env.stopped(self.clock.now_ms().get());
        let git = Git {
            runner: &*self.helpers,
            ctx,
            program: &self.config.program,
            deadline: self.config.deadline,
            stop: &stopped,
        };
        let paths = input
            .paths
            .0
            .iter()
            .map(|p| p.0.clone())
            .collect::<Vec<_>>();
        self.verify_target(&git, &target)
            .map_err(|e| read_failure(e, &target.repository.id))?;
        let bytes = git
            .patch(target.path(), &input.base.hex, &input.head.hex, &paths)
            .map_err(|e| read_failure(e, &target.repository.id))?;
        let mut items = Vec::new();
        if !bytes.is_empty() {
            if stopped() {
                return Err(crate::ports::storage::StorageError::Cancelled.into());
            }
            let project = match (ctx.project_id(), target.worktree.as_ref()) {
                (Some(p), _) => Some(p.clone()),
                (None, Some(tree)) => env
                    .read(|s| s.work().work(&tree.work_id))?
                    .map(|w| w.project_id),
                _ => None,
            };
            let evidence = Evidence {
                scope: project.map_or(ArtifactScope::Workspace, ArtifactScope::Project),
                producer: ctx.principal().clone(),
                principal_kind: format!("{:?}", ctx.principal_kind()),
                agent_session: ctx.session_id().cloned(),
                root: ctx.root_id().cloned(),
                assignment: ctx.assignment_id().cloned(),
                title: format!(
                    "Git diff {}..{}",
                    &input.base.hex[..12],
                    &input.head.hex[..12]
                ),
                media_type: "text/x-diff".into(),
                tags: vec!["git".into(), "diff".into()],
                source: serde_json::json!({"kind":"GitDiff","repositoryId":target.repository.id.as_str(),"worktreeId":target.worktree.as_ref().map(|w|w.id.as_str()),"base":input.base,"head":input.head,"paths":paths}),
            };
            let artifact = self.publish_read(ctx, &target, query, env, evidence, &bytes)?;
            items.push(artifact);
        }
        Ok(PreparedRead {
            target,
            result: OperationResult::GitDiff {
                value: crate::protocol::support::Page {
                    items: crate::protocol::support::BoundedList(items),
                    next_cursor: None,
                    has_more: false,
                },
            },
        })
    }
}
impl QueryHandler for GitService {
    fn prepare(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, DomainFault> {
        let (repository, worktree) = match query.input() {
            OperationInput::Git(ops::Input::Inspect(i)) => {
                (&i.repository_id, i.worktree_id.as_ref())
            }
            OperationInput::Git(ops::Input::Diff(i)) => (&i.repository_id, i.worktree_id.as_ref()),
            _ => return Ok(None),
        };
        let result = (|| {
            let target =
                env.read(|snapshot| Ok(self.target(snapshot, ctx, repository, worktree)))??;
            match query.input() {
                OperationInput::Git(ops::Input::Diff(i)) => self.diff(ctx, target, i, query, env),
                _ => self.inspect(ctx, target, query, env).map(|v| PreparedRead {
                    target: v.target,
                    result: OperationResult::GitInspect { value: v.value },
                }),
            }
        })();
        result
            .map(|value| Some(Prepared::new(value)))
            .map_err(|f| query_fault(f, "git_repository", self.correlation()))
    }
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let result = (|| {
            let observed = query.prepared::<PreparedRead>().ok_or_else(|| {
                refuse(
                    DomainErrorCode::UnsupportedCapability,
                    "git_operation",
                    &Id::new("git").expect("constant"),
                )
            })?;
            let old = &observed.target;
            let current = self.target(
                snapshot,
                ctx,
                &old.repository.id,
                old.worktree.as_ref().map(|w| &w.id),
            )?;
            if current.repository.revision != old.repository.revision
                || current.worktree != old.worktree
            {
                return Err(refuse(
                    DomainErrorCode::Conflict,
                    "git_ownership",
                    &old.repository.id,
                ));
            }
            Ok(observed.result.clone())
        })()
        .map_err(|f| query_fault(f, "git_repository", self.correlation()))?;
        Ok(QueryAnswer {
            stamp: snapshot
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(1)),
            result,
        })
    }
}

impl GitService {
    /// Resolve current source from the host's ownership witness. None means explicitly
    /// unbound; a stale, removed or unreadable binding is a refusal, never a guessed source.
    pub fn observe_source(
        &self,
        ctx: &InvocationContext,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Option<serde_json::Value>, Failure> {
        let Some(binding) = ctx.worktree() else {
            return Ok(None);
        };
        let started = std::time::Instant::now();
        let stopped = || self.is_stopped() || stop() || started.elapsed() >= self.config.deadline;
        let read_error = |error| {
            read_failure(
                if started.elapsed() >= self.config.deadline {
                    GitFault::Helper(HelperFault::TimedOut)
                } else {
                    error
                },
                binding.id(),
            )
        };
        let rebound = context::bind_worktree(&*self.store, ctx.clone(), binding.id())?;
        if rebound.worktree() != Some(binding) {
            return Err(refuse(
                DomainErrorCode::SourceChanged,
                "git_ownership",
                binding.id(),
            ));
        }
        let target = crate::ports::storage::read_snapshot(
            &*self.store,
            &crate::ports::storage::ReadOptions::default(),
            |s| Ok(self.target(s, ctx, binding.repository_id(), Some(binding.id()))),
        )??;
        let git = Git {
            runner: &*self.helpers,
            ctx,
            program: &self.config.program,
            deadline: self.config.deadline,
            stop: &stopped,
        };
        self.verify_target(&git, &target).map_err(read_error)?;
        let source = git
            .revision_observation(target.path())
            .map_err(read_error)?;
        let current = crate::ports::storage::read_snapshot(
            &*self.store,
            &crate::ports::storage::ReadOptions::default(),
            |s| {
                Ok((
                    s.git().git_worktree(binding.id())?,
                    s.projects()
                        .repository(binding.repository_id())?
                        .map(|r| r.revision),
                ))
            },
        )?;
        let rebound = context::bind_worktree(&*self.store, ctx.clone(), binding.id())?;
        if rebound.worktree() != Some(binding)
            || current.0.as_ref() != target.worktree.as_ref()
            || current.1 != Some(target.repository.revision)
        {
            return Err(refuse(
                DomainErrorCode::SourceChanged,
                "git_source",
                binding.id(),
            ));
        }
        let mut value = serde_json::json!({"repositoryId":binding.repository_id(),"worktreeId":binding.id(),"head":{"format":if source.head.len()==64 {"Sha256"} else {"Sha1"},"hex":source.head},"dirty":source.dirty,"ownershipRevision":binding.ownership_revision().to_string(),"observedAtMs":self.clock.now_ms().get().to_string()});
        if let Some(repository) = target
            .repository
            .remote_url
            .as_deref()
            .and_then(github::Repository::from_remote)
        {
            value["repositoryUrl"] = serde_json::json!(format!(
                "https://github.com/{}/{}",
                repository.owner, repository.name
            ));
        }
        Ok(Some(value))
    }
}
