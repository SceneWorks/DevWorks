//! Task PR creation/update and current-membership target reconciliation.
use super::*;
use crate::services::git::github::{Fault, PullRequest, Repository};

pub(super) struct StagedPr(Result<PrInput, Failure>);
struct PrInput {
    tree: StoredGitWorktree,
    repository: StoredRepository,
    remote: Repository,
    default_branch: String,
    description: String,
}
fn api_failure(error: Fault, id: &Id) -> Failure {
    let code = match error {
        Fault::CredentialsRequired | Fault::Rejected(401) => DomainErrorCode::CredentialsRequired,
        Fault::Rejected(403) => DomainErrorCode::PermissionDenied,
        Fault::Ambiguous => DomainErrorCode::Conflict,
        _ => DomainErrorCode::ExternalFailure,
    };
    refuse(code, "github_pr", id)
}
impl GitService {
    pub(super) fn pr_manager(
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        work: &StoredWork,
    ) -> Result<bool, StorageError> {
        use crate::settings::catalog::AgentRole;
        if matches!(
            ctx.principal_kind(),
            PrincipalKind::Human | PrincipalKind::Host
        ) {
            return Ok(true);
        }
        let principal = tx.authority().principal(ctx.principal())?;
        Ok(principal.is_some_and(|p| {
            p.role == Some(AgentRole::WorkspaceCoordinator)
                || (p.role == Some(AgentRole::FeatureCoordinator)
                    && work.parent_feature_id.as_ref().is_some_and(|feature| {
                        ctx.root_id().is_some_and(|root| &root.0 == feature)
                    }))
        }))
    }
    fn pr_command(
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
        if tree.owner == *ctx.principal() {
            return self.owned_command(tx, ctx, id);
        }
        let repository = tx
            .projects()
            .repository(&tree.repository_id)?
            .ok_or_else(|| refuse(DomainErrorCode::NotFound, "repository", &tree.repository_id))?;
        if tree.removed || tree.pending_operation_id.is_some() || !Self::pr_manager(tx, ctx, &work)?
        {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                "github_pr_management",
                id,
            ));
        }
        self.check_work_scope(tx, ctx, &work, &repository, false)?;
        Ok((tree, repository, work))
    }

    pub(super) fn prepare_pull_request(
        &self,
        ctx: &InvocationContext,
        input: &GitCommand,
        env: &Preparation<'_>,
    ) -> Result<Option<Prepared>, StorageError> {
        let GitCommand::OpenPullRequest {
            repository_id,
            head_ref,
            description,
            ..
        } = input
        else {
            return Ok(None);
        };
        let prepared = (|| {
            let (tree, repository) = env.read(|s| {
                let tree = s
                    .git()
                    .git_worktree_for_branch(repository_id, head_ref)?
                    .ok_or(StorageError::ScopeDenied)?;
                Ok(self
                    .target(s, ctx, repository_id, Some(&tree.id))
                    .map(|target| (tree, target.repository)))
            })??;
            let client = self.config.github.as_ref().ok_or_else(|| {
                refuse(DomainErrorCode::UnsupportedTarget, "github", repository_id)
            })?;
            let remote = repository
                .remote_url
                .as_deref()
                .and_then(Repository::from_remote)
                .ok_or_else(|| {
                    refuse(
                        DomainErrorCode::UnsupportedTarget,
                        "github_repository",
                        repository_id,
                    )
                })?;
            let mut source = self
                .artifacts
                .open_exact_version(&*self.store, ctx, description)
                .map_err(|r| refuse(r.code, "git_pr_description", &description.version_id))?;
            let mut bytes = Vec::new();
            source
                .reader
                .by_ref()
                .take(64 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| StorageError::unavailable(Unavailable::Io, e.to_string()))?;
            if bytes.len() > 64 * 1024 || bytes.contains(&0) {
                return Err(refuse(
                    DomainErrorCode::InvalidArgument,
                    "git_pr_description",
                    &description.version_id,
                ));
            }
            let description = String::from_utf8(bytes).map_err(|_| {
                refuse(
                    DomainErrorCode::InvalidArgument,
                    "git_pr_description",
                    &description.version_id,
                )
            })?;
            if env.stopped(self.clock.now_ms().get()) {
                return Err(StorageError::Cancelled.into());
            }
            let default_branch = client
                .default_branch(&*self.store, &remote)
                .map_err(|e| api_failure(e, repository_id))?;
            Ok(PrInput {
                tree,
                repository,
                remote,
                default_branch,
                description,
            })
        })();
        Ok(Some(Prepared::new(StagedPr(prepared))))
    }

    pub(super) fn decide_pull_request(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
        input: &GitCommand,
    ) -> Result<(StoredGitWorktree, StoredRepository, StoredWork, Action), Failure> {
        let GitCommand::OpenPullRequest {
            repository_id,
            head_ref,
            base_ref,
            expected_head,
            covered_work_ids,
            ..
        } = input
        else {
            unreachable!("selected PR")
        };
        let staged = command
            .prepared::<StagedPr>()
            .ok_or_else(|| refuse(DomainErrorCode::Conflict, "github_pr", repository_id))?;
        let staged = match &staged.0 {
            Ok(staged) => staged,
            Err(Failure::Refused(r)) => return Err(r.clone().into()),
            Err(_) => {
                return Err(refuse(
                    DomainErrorCode::ExternalFailure,
                    "github_pr",
                    repository_id,
                ));
            }
        };
        let (tree, repository, work) = self.pr_command(tx, ctx, &staged.tree.id)?;
        if tree != staged.tree
            || repository.revision != staged.repository.revision
            || &tree.head != expected_head
            || &tree.branch != head_ref
        {
            return Err(refuse(
                DomainErrorCode::SourceChanged,
                "github_pr_head",
                repository_id,
            ));
        }
        // Final feature PRs are admitted by the separate delivery barrier owner (DW-39), not
        // by this task-PR operation. This prevents an early task delivery path to main.
        if work.kind == FEATURE {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                "feature_delivery_barrier",
                &work.id,
            ));
        }
        let base = match &work.parent_feature_id {
            None => format!("refs/heads/{}", staged.default_branch),
            Some(feature) => {
                let trees = tx.git().git_worktrees(feature, None, MAX_PAGE)?;
                let matches: Vec<_> = trees
                    .iter()
                    .filter(|t| {
                        t.repository_id == repository.id
                            && !t.removed
                            && t.pending_operation_id.is_none()
                    })
                    .collect();
                let [feature_tree] = matches.as_slice() else {
                    return Err(refuse(DomainErrorCode::Conflict, "feature_branch", feature));
                };
                feature_tree.branch.clone()
            }
        };
        if base_ref != &base || base_ref == head_ref {
            return Err(refuse(
                DomainErrorCode::InvalidArgument,
                "github_pr_base",
                &work.id,
            ));
        }
        let ids = covered_work_ids
            .clone()
            .unwrap_or_else(|| vec![work.id.clone()]);
        if !ids.contains(&work.id) || (ids.len() > 1 && !Self::pr_manager(tx, ctx, &work)?) {
            return Err(refuse(
                DomainErrorCode::PermissionDenied,
                "github_pr_coverage",
                &work.id,
            ));
        }
        let mut covered = Vec::new();
        for id in ids {
            let member = tx
                .work()
                .work(&id)?
                .ok_or_else(|| refuse(DomainErrorCode::NotFound, "work", &id))?;
            if member.project_id != work.project_id {
                return Err(refuse(DomainErrorCode::NotFound, "work", &id));
            }
            if !matches!(member.kind.as_str(), TASK | "issue")
                || member.project_id != work.project_id
                || member.parent_feature_id != work.parent_feature_id
                || !tx.work().work_repositories(&id)?.contains(&repository.id)
            {
                return Err(refuse(
                    DomainErrorCode::InvalidArgument,
                    "github_pr_coverage",
                    &id,
                ));
            }
            covered.push((id, member.revision));
        }
        Ok((
            tree,
            repository,
            work.clone(),
            Action::PullRequest {
                repository: staged.remote.clone(),
                head: head_ref
                    .strip_prefix("refs/heads/")
                    .expect("validated ref")
                    .into(),
                base: base
                    .strip_prefix("refs/heads/")
                    .expect("validated ref")
                    .into(),
                title: work.title,
                description: staged.description.clone(),
                expected: expected_head.clone(),
                covered,
            },
        ))
    }

    fn pr_matches(job: &Job, pr: &PullRequest) -> bool {
        matches!(&job.action, Action::PullRequest { head, base, title, description, expected, .. }
            if &pr.head == head && &pr.base == base && pr.sha == expected.hex && &pr.title == title && &pr.body == description)
    }

    pub(super) fn finish_pull_request(
        &self,
        job: &Job,
        operation: &OperationId,
        pr: PullRequest,
    ) -> EffectOutcome {
        if !Self::pr_matches(job, &pr) {
            return EffectOutcome::Unknown(
                "PR postcondition does not match the requested head, base and description".into(),
            );
        }
        let value = crate::protocol::records::GitLink {
            repository_id: job.tree.repository_id.clone(),
            work_id: job.tree.work_id.clone(),
            worktree_id: Some(job.tree.id.clone()),
            branch: Some(crate::protocol::support::BoundedText(
                job.tree.branch.clone(),
            )),
            pull_request_url: Some(
                serde_json::from_value(json!(pr.url)).expect("constructed GitHub.com PR URL"),
            ),
            head: job.tree.head.clone(),
        };
        let result = json!({"result":OperationResult::GitPullRequest { value }});
        let checkpoint = result.clone();
        let (j, op) = (job.clone(), operation.clone());
        match run_write(&*self.store, WriteOptions::default(), move |tx| {
            let mut tree = tx
                .git()
                .git_worktree(&j.tree.id)?
                .ok_or(StorageError::ScopeDenied)?;
            if tree.last_operation_id.as_ref() == Some(&op) && tree.pending_operation_id.is_none() {
                return Ok(());
            }
            if tree != j.tree {
                return Err(StorageError::ScopeDenied);
            }
            let prior = tree.revision;
            tree.revision += 1;
            tree.pending_operation_id = None;
            tree.last_operation_id = Some(op.clone());
            tree.last_result = Some(checkpoint.clone());
            tx.git().update_git_worktree(&tree, prior)?;
            let event = EventId(tx.new_id(IdKind::Event));
            let sequence = tx.history().append(NewEvent { id: event.clone(), actor: j.actor.clone(), project_id: Some(j.project.clone()), root_id: j.root.clone(), operation_id: Some(op.clone()), aggregate_kind: "git_worktree".into(), aggregate_id: tree.id.as_str().into(), aggregate_revision: tree.revision, kind: "GitPullRequestObserved".into(), payload: json!({"repositoryId":tree.repository_id,"workId":tree.work_id,"worktreeId":tree.id,"branch":tree.branch,"head":tree.head,"pullRequestUrl":pr.url}) })?;
            let mut roots = match &j.action {
                Action::PullRequest { covered, .. } => covered
                    .iter()
                    .map(|(id, _)| RootId(id.clone()))
                    .collect::<Vec<_>>(),
                _ => vec![RootId(tree.work_id.clone())],
            };
            if let Some(feature) = &j.feature_id {
                roots.push(RootId(feature.clone()));
            }
            if let Some(root) = &j.root
                && !roots.contains(root)
            {
                roots.push(root.clone());
            }
            for root in roots {
                let link = tx.new_id(IdKind::Link);
                let now = tx.now_ms();
                tx.sessions().insert_link(&StoredLink { id: link, project_id: j.project.clone(), root_id: root, kind: "pull_request".into(), target_kind: "pull_request".into(), target_id: pr.url.clone(), parent_id: None, supersedes_id: None, actor: j.actor.clone(), event_id: event.0.clone(), sequence, repository_id: Some(tree.repository_id.clone()), revision: Some(tree.head.hex.clone()), summary: format!("Task PR {}", pr.url), body: json!({"agentSessionId":j.session,"worktreeId":tree.id,"branch":tree.branch,"pullRequestUrl":pr.url,"head":tree.head}), created_at_ms: now })?;
            }
            Ok(())
        }) {
            Ok(()) => EffectOutcome::Completed(result),
            Err(e) => storage_failure(e),
        }
    }

    pub(super) fn execute_pull_request(
        &self,
        job: &Job,
        operation: &OperationRecord,
        git: &Git<'_>,
        path: &Path,
    ) -> EffectOutcome {
        let Action::PullRequest {
            repository,
            head,
            base,
            title,
            description,
            expected,
            ..
        } = &job.action
        else {
            unreachable!("PR effect")
        };
        let Some(client) = &self.config.github else {
            return denied("GitHub client unavailable");
        };
        if !matches!(git.revision_observation(path), Ok(o) if o.head == expected.hex && o.branch.as_deref() == Some(&job.tree.branch))
        {
            return denied("local PR head changed");
        }
        if self.git_stopped(job) {
            return denied("work stopped before PR dispatch");
        }
        let existing = match client.find(&*self.store, repository, head) {
            Ok(pr) => pr,
            Err(_) => return denied("PR state unavailable before dispatch"),
        };
        if self.git_stopped(job) {
            return denied("work or authority changed before PR write");
        }
        let written = match existing {
            Some(pr) if Self::pr_matches(job, &pr) => {
                return self.finish_pull_request(job, &operation.id, pr);
            }
            Some(pr) => {
                if pr.sha != expected.hex {
                    return denied("remote PR head changed");
                }
                client.update(
                    &*self.store,
                    repository,
                    pr.number,
                    base,
                    title,
                    description,
                )
            }
            None => client.create(&*self.store, repository, head, base, title, description),
        };
        // A create/update response alone is not the verification read. Transport loss is
        // reconciled with the same read; it never causes another POST/PATCH in this attempt.
        let observed = client.find(&*self.store, repository, head);
        match observed {
            Ok(Some(pr)) if Self::pr_matches(job, &pr) => {
                self.finish_pull_request(job, &operation.id, pr)
            }
            Ok(None) if matches!(written, Err(Fault::Rejected(400 | 401 | 403 | 404 | 422))) => {
                denied("GitHub rejected the PR write")
            }
            _ => EffectOutcome::Unknown(
                "GitHub PR outcome is unverified; the write was not repeated".into(),
            ),
        }
    }
}
