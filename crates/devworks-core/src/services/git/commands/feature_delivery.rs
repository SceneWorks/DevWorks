//! Shared prerequisites for local feature delivery.
use super::*;

/// A worktree alone does not establish a feature branch on the registered remote. Require the
/// Git owner's durable observation before creating a task from that published commit. Later
/// inspections may have replaced the worktree's most recent operation, so use its history.
pub(super) fn require_published_branch(
    tx: &mut dyn WriteTransaction,
    feature: &Id,
    repository: &Id,
    base: &GitOid,
) -> Result<(), Failure> {
    let mut after = None;
    loop {
        let page = tx.git().git_worktrees(feature, after.as_ref(), MAX_PAGE)?;
        for tree in &page {
            if &tree.repository_id == repository
                && !tree.removed
                && tree.pending_operation_id.is_none()
                && &tree.head == base
                && observed_published_head(tx, tree)?
            {
                return Ok(());
            }
        }
        if page.len() < usize::from(MAX_PAGE) {
            break;
        }
        after = page.last().map(|tree| tree.id.clone());
    }
    Err(refuse(
        DomainErrorCode::Conflict,
        "published_feature_branch",
        feature,
    ))
}

pub(super) fn observed_published_head(
    tx: &mut dyn WriteTransaction,
    tree: &StoredGitWorktree,
) -> Result<bool, StorageError> {
    observed_publication(tx, tree, &tree.head)
}

pub(super) fn observed_publication(
    tx: &mut dyn WriteTransaction,
    tree: &StoredGitWorktree,
    head: &GitOid,
) -> Result<bool, StorageError> {
    let filter = EventFilter::Aggregate {
        kind: "git_worktree".into(),
        id: tree.id.as_str().to_owned(),
    };
    let Some(repository) = tx
        .projects()
        .repository(&tree.repository_id)?
        .filter(|repository| repository.lifecycle == ACTIVE)
    else {
        return Ok(false);
    };
    let Some(remote) = repository.remote_url else {
        return Ok(false);
    };
    let exact_head = serde_json::to_value(head)
        .map_err(|_| StorageError::InvalidRecord("feature branch head".into()))?;
    let mut after = 0;
    loop {
        let page = tx.history().page(&filter, after, MAX_PAGE)?;
        for event in &page.events {
            if event.kind != "GitWriteObserved"
                || event.payload["head"] != exact_head
                || event.payload["branch"] != tree.branch
                || event.payload["repositoryId"].as_str() != Some(tree.repository_id.as_str())
            {
                continue;
            }
            let Some(id) = &event.operation_id else {
                continue;
            };
            if tx.operations().operation(id)?.is_some_and(|operation| {
                operation.capability == "git.push"
                    && operation.state == OperationState::ObservedComplete
                    && operation.payload["repository_revision"].as_i64()
                        == Some(repository.revision)
                    && operation.payload["action"]["Push"]["url"].as_str() == Some(remote.as_str())
                    && operation.payload["action"]["Push"]["target"].as_str()
                        == Some(tree.branch.as_str())
                    && operation.payload["action"]["Push"]["expected"] == exact_head
            }) {
                return Ok(true);
            }
        }
        if !page.has_more {
            return Ok(false);
        }
        after = page.next_after;
    }
}

/// Once the final feature PR exists, its exact-head review service owns fixes to that PR.
/// Before it exists, every changed feature-branch push must come from task integration.
pub(super) fn has_final_pull_request(
    tx: &mut dyn WriteTransaction,
    tree: &StoredGitWorktree,
    repository_revision: i64,
) -> Result<bool, StorageError> {
    let filter = EventFilter::Aggregate {
        kind: "git_worktree".into(),
        id: tree.id.as_str().to_owned(),
    };
    let mut after = 0;
    loop {
        let page = tx.history().page(&filter, after, MAX_PAGE)?;
        for event in &page.events {
            if event.kind != "GitPullRequestObserved" || event.payload["branch"] != tree.branch {
                continue;
            }
            let Some(id) = &event.operation_id else {
                continue;
            };
            if tx.operations().operation(id)?.is_some_and(|operation| {
                operation.capability == "git.pull_request"
                    && operation.state == OperationState::ObservedComplete
                    && operation.payload["repository_revision"].as_i64()
                        == Some(repository_revision)
                    && operation.payload["action"]["PullRequest"]["final_feature"] == true
            }) {
                return Ok(true);
            }
        }
        if !page.has_more {
            return Ok(false);
        }
        after = page.next_after;
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct TaskPublication {
    pub proof: crate::protocol::data::FeatureTaskIntegration,
    pub review_revision: i64,
    pub task_worktree: StoredGitWorktree,
    pub delta: crate::protocol::provider::ArtifactRef,
}

pub(super) struct PreparedPublication {
    candidate: GitOid,
    review_revision: i64,
    task_worktree: StoredGitWorktree,
    pub(super) diff: crate::services::artifacts::StagedFile,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Verification {
    task_id: Id,
    review_id: Id,
    repository_id: Id,
    predecessor: GitOid,
    task_head: GitOid,
    candidate: GitOid,
    preserves_task_behavior: bool,
    preserves_feature_behavior: bool,
    focused_checks: Vec<Id>,
}

impl GitService {
    pub(super) fn prepare_feature_publication(
        &self,
        ctx: &InvocationContext,
        env: &Preparation<'_>,
        git: &Git<'_>,
        tree: &StoredGitWorktree,
        candidate: &GitOid,
        proof: &crate::protocol::data::FeatureTaskIntegration,
    ) -> Result<PreparedPublication, Failure> {
        let invalid = || {
            refuse(
                DomainErrorCode::SourceChanged,
                "feature_integration",
                &proof.task_id,
            )
        };
        let root = RootId(tree.work_id.clone());
        let (review_revision, task_worktree) = env.read(|snapshot| {
            let revision = crate::services::reviews::integration_review(
                snapshot,
                ctx,
                &root,
                &tree.repository_id,
                proof,
            )
            .map_err(Failure::into_storage)?;
            let worktree = snapshot
                .git()
                .git_worktree(&proof.task_worktree_id)?
                .ok_or(StorageError::ScopeDenied)?;
            Ok((revision, worktree))
        })?;
        if task_worktree.head != proof.task_head
            || task_worktree
                .location
                .as_ref()
                .zip(tree.location.as_ref())
                .is_none_or(|(task, feature)| task.common_dir != feature.common_dir)
        {
            return Err(invalid());
        }
        let (bytes, _) = self
            .artifacts
            .read_exact(self.store.as_ref(), ctx, &proof.verification, 128 * 1024)
            .map_err(|_| invalid())?;
        let check: Verification = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if check.task_id != proof.task_id
            || check.review_id != proof.review_id
            || check.repository_id != tree.repository_id
            || check.predecessor != proof.predecessor
            || check.task_head != proof.task_head
            || &check.candidate != candidate
            || !check.preserves_task_behavior
            || !check.preserves_feature_behavior
            || check.focused_checks != proof.focused_checks
        {
            return Err(invalid());
        }
        let path = Path::new(&tree.path);
        let correct_parents = if candidate == &proof.task_head {
            git.is_ancestor(path, &proof.predecessor.hex, &candidate.hex) == Ok(true)
        } else {
            git.commit_parents(path, &candidate.hex).as_deref()
                == Ok(&[proof.predecessor.hex.clone(), proof.task_head.hex.clone()])
        };
        if !correct_parents {
            return Err(invalid());
        }
        let mut delta = serde_json::Map::new();
        for (name, parent) in [
            ("featureDelta", &proof.predecessor),
            ("taskDelta", &proof.task_head),
        ] {
            let patch = git
                .review_patch(path, &parent.hex, &candidate.hex)
                .map_err(|error| read_failure(error, &tree.repository_id))?;
            let patch = String::from_utf8(patch).map_err(|_| invalid())?;
            delta.insert(name.into(), json!(patch));
        }
        let bytes = serde_json::to_vec(&json!({
            "repositoryId":tree.repository_id,"taskId":proof.task_id,"reviewId":proof.review_id,
            "reviewedBase":proof.reviewed_base,"reviewedHead":proof.reviewed_head,"taskHead":proof.task_head,
            "predecessor":proof.predecessor,"candidate":candidate,"patches":delta
        })).map_err(|_| invalid())?;
        let diff = self
            .artifacts
            .stage_host_bytes(
                self.store.as_ref(),
                &bytes,
                json!({"kind":"FeatureIntegrationDiff","taskId":proof.task_id}),
            )?
            .map_err(|_| invalid())?;
        Ok(PreparedPublication {
            candidate: candidate.clone(),
            review_revision,
            task_worktree,
            diff,
        })
    }
}

pub(super) fn persist_task_publication(
    service: &GitService,
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    feature: &StoredWork,
    tree: &StoredGitWorktree,
    proof: &crate::protocol::data::FeatureTaskIntegration,
    prepared: &PreparedPublication,
) -> Result<TaskPublication, Failure> {
    let invalid = || {
        refuse(
            DomainErrorCode::SourceChanged,
            "feature_integration",
            &proof.task_id,
        )
    };
    let task = tx.work().work(&proof.task_id)?.ok_or_else(invalid)?;
    if feature.kind != FEATURE
        || task.parent_feature_id.as_ref() != Some(&feature.id)
        || task.project_id != feature.project_id
        || task.revision != proof.expected_task_revision.get()
        || !tx
            .work()
            .work_repositories(&task.id)?
            .contains(&tree.repository_id)
        || tx.git().git_worktree(&proof.task_worktree_id)?.as_ref() != Some(&prepared.task_worktree)
        || tx
            .git()
            .code_review(&proof.review_id)?
            .is_none_or(|review| review.revision != prepared.review_revision)
        || !observed_publication(tx, tree, &proof.predecessor)?
    {
        return Err(invalid());
    }
    crate::services::work::check_artifact(tx, ctx, &feature.project_id, &proof.verification)?;
    if tx
        .artifacts()
        .artifact(&proof.verification.artifact_id)?
        .is_none_or(|artifact| {
            artifact.scope.project() != Some(&feature.project_id)
                || (artifact.owner.as_ref() != Some(ctx.principal())
                    && (artifact.origin_work_id.as_ref() != Some(&RootId(feature.id.clone()))
                        || artifact.provenance["producedBy"].as_str()
                            != Some(ctx.principal().0.as_str())))
        })
    {
        return Err(invalid());
    }
    for id in &proof.focused_checks {
        let operation = tx
            .operations()
            .operation(&OperationId(id.clone()))?
            .ok_or_else(invalid)?;
        if operation
            .root_id
            .as_ref()
            .is_none_or(|root| root.0 != feature.id)
            || operation.capability != "process.execute"
            || operation.state != OperationState::ObservedComplete
            || !operation.latest_attempt().is_some_and(|attempt| {
                attempt.observations.iter().any(|note| {
                    let evidence = &note["effect"]["evidence"];
                    let checked = &evidence["verifiedWorktree"];
                    evidence["result"]["value"]["exitCode"] == 0
                        && checked["worktreeId"].as_str() == Some(tree.id.as_str())
                        && checked["repositoryId"].as_str() == Some(tree.repository_id.as_str())
                        && checked["head"] == json!(prepared.candidate)
                        && checked["worktreeRevision"].as_i64() == Some(tree.revision)
                })
            })
        {
            return Err(invalid());
        }
    }
    let version = service.artifacts.commit_host_bytes_in_scope(
        tx,&prepared.diff,crate::services::artifacts::host::HostArtifactPublication {
            title:"Feature task integration diff",
            media_type:"application/json",
            provenance:json!({"kind":"FeatureIntegrationDiff","taskId":task.id,"candidate":prepared.candidate}),
            scope:ArtifactScope::Project(feature.project_id.clone()),owner:ctx.principal().clone(),
        }
    )?;
    Ok(TaskPublication {
        proof: proof.clone(),
        review_revision: prepared.review_revision,
        task_worktree: prepared.task_worktree.clone(),
        delta: crate::services::artifacts::version_ref(&version),
    })
}

pub(super) fn uses_local_integration(
    tx: &mut dyn WriteTransaction,
    feature: &Id,
    repository: &Id,
) -> Result<bool, StorageError> {
    let mut after = None;
    loop {
        let page = tx.git().git_worktrees(feature, after.as_ref(), MAX_PAGE)?;
        if page.iter().any(|tree| {
            &tree.repository_id == repository
                && !tree.removed
                && tree.branch.starts_with("refs/heads/feature/")
        }) {
            return Ok(true);
        }
        if page.len() < usize::from(MAX_PAGE) {
            return Ok(false);
        }
        after = page.last().map(|tree| tree.id.clone());
    }
}

pub(crate) fn requires_local_integration(
    tx: &mut dyn WriteTransaction,
    feature: &Id,
    repositories: &[Id],
) -> Result<bool, StorageError> {
    for repository in repositories {
        if uses_local_integration(tx, feature, repository)? {
            return Ok(true);
        }
    }
    Ok(false)
}

impl TaskPublication {
    fn task_matches(&self, task: &StoredWork, feature: &Id, project: &ProjectId) -> bool {
        task.parent_feature_id.as_ref() == Some(feature)
            && &task.project_id == project
            && task.revision == self.proof.expected_task_revision.get()
    }

    fn source_matches(&self, git: &dyn GitRead) -> Result<bool, StorageError> {
        Ok(
            git.git_worktree(&self.proof.task_worktree_id)?.as_ref() == Some(&self.task_worktree)
                && git
                    .code_review(&self.proof.review_id)?
                    .is_some_and(|row| row.revision == self.review_revision),
        )
    }

    pub(super) fn current_in(
        &self,
        snapshot: &dyn ReadSnapshot,
        job: &Job,
    ) -> Result<bool, StorageError> {
        Ok(snapshot
            .work()
            .work(&self.proof.task_id)?
            .is_some_and(|task| self.task_matches(&task, &job.tree.work_id, &job.project))
            && snapshot
                .work()
                .work_repositories(&self.proof.task_id)?
                .contains(&job.tree.repository_id)
            && self.source_matches(snapshot.git())?)
    }
}

/// Keep the immutable task source and current review bound through dispatch.
pub(super) fn publication_current(
    tx: &mut dyn WriteTransaction,
    feature: &StoredWork,
    tree: &StoredGitWorktree,
    publication: &TaskPublication,
) -> Result<(), StorageError> {
    let proof = &publication.proof;
    if !tx
        .work()
        .work(&proof.task_id)?
        .is_some_and(|task| publication.task_matches(&task, &feature.id, &feature.project_id))
        || !tx
            .work()
            .work_repositories(&proof.task_id)?
            .contains(&tree.repository_id)
        || !publication.source_matches(tx.git())?
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}

const DELIVERY_AGGREGATE: &str = "feature_task_publication";

fn publication_history(task: &Id, repository: &Id) -> EventFilter {
    EventFilter::Aggregate {
        kind: DELIVERY_AGGREGATE.into(),
        id: crate::fingerprint::sha256_hex(
            format!("{}/{}", task.as_str(), repository.as_str()).as_bytes(),
        ),
    }
}

pub(super) fn published_task(
    tx: &mut dyn WriteTransaction,
    task: &Id,
    repository: &Id,
) -> Result<Option<EventRecord>, StorageError> {
    let filter = publication_history(task, repository);
    let mut after = 0;
    let mut latest = None;
    loop {
        let page = tx.history().page(&filter, after, MAX_PAGE)?;
        for event in page.events {
            if event.kind == "FeatureTaskPublished" {
                latest = Some(event);
            }
        }
        if !page.has_more {
            return Ok(latest);
        }
        after = page.next_after;
    }
}

fn registered_publication(
    tx: &mut dyn WriteTransaction,
    task: &Id,
    repository: &Id,
) -> Result<Option<EventRecord>, StorageError> {
    let Some(registration) = tx
        .projects()
        .repository(repository)?
        .filter(|registration| registration.lifecycle == ACTIVE)
    else {
        return Ok(None);
    };
    Ok(published_task(tx, task, repository)?.filter(|publication| {
        publication.payload["repositoryRevision"].as_i64() == Some(registration.revision)
    }))
}

pub(super) fn settle_task_publication(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    tree: &StoredGitWorktree,
    operation: &OperationId,
    publication: &TaskPublication,
) -> Result<(), StorageError> {
    let proof = &publication.proof;
    let EventFilter::Aggregate { kind, id } =
        publication_history(&proof.task_id, &tree.repository_id)
    else {
        unreachable!("feature task aggregate")
    };
    let prior = published_task(tx, &proof.task_id, &tree.repository_id)?;
    if prior.as_ref().and_then(|event| event.operation_id.as_ref()) == Some(operation) {
        return Ok(());
    }
    let task = tx
        .work()
        .work(&proof.task_id)?
        .ok_or(StorageError::ScopeDenied)?;
    let repository_revision = tx
        .operations()
        .operation(operation)?
        .and_then(|operation| operation.payload["repository_revision"].as_i64())
        .ok_or(StorageError::ScopeDenied)?;
    let owner = ctx.principal().clone();
    let payload = json!({
        "featureId":tree.work_id,"taskId":task.id,"taskRevision":proof.expected_task_revision,
        "repositoryId":tree.repository_id,"worktreeId":tree.id,"branch":tree.branch,"repositoryRevision":repository_revision,
        "reviewId":proof.review_id,"reviewedBase":proof.reviewed_base,"reviewedHead":proof.reviewed_head,
        "taskWorktreeId":proof.task_worktree_id,"taskHead":proof.task_head,"predecessor":proof.predecessor,"candidate":tree.head,"head":tree.head,
        "resolutionDiff":publication.delta,"verification":proof.verification,"focusedChecks":proof.focused_checks,
    });
    let event = EventId(tx.new_id(IdKind::Event));
    let sequence = tx.history().append(NewEvent {
        id: event.clone(),
        actor: owner.clone(),
        project_id: Some(task.project_id.clone()),
        root_id: Some(RootId(tree.work_id.clone())),
        operation_id: Some(operation.clone()),
        aggregate_kind: kind,
        aggregate_id: id,
        aggregate_revision: prior
            .as_ref()
            .map_or(1, |event| event.aggregate_revision + 1),
        kind: "FeatureTaskPublished".into(),
        payload: payload.clone(),
    })?;
    for root in [RootId(task.id.clone()), RootId(tree.work_id.clone())] {
        let link = tx.new_id(IdKind::Link);
        let now = tx.now_ms();
        tx.sessions().insert_link(&StoredLink {
            id: link,
            project_id: task.project_id.clone(),
            root_id: root.clone(),
            kind: "git".into(),
            target_kind: "git_worktree".into(),
            target_id: tree.id.as_str().into(),
            parent_id: None,
            supersedes_id: None,
            actor: owner.clone(),
            event_id: event.0.clone(),
            sequence,
            repository_id: Some(tree.repository_id.clone()),
            revision: Some(tree.head.hex.clone()),
            summary: format!("Task {} integrated into {}", task.id.as_str(), tree.branch),
            body: payload.clone(),
            created_at_ms: now,
        })?;
        for (artifact, summary) in [
            (&publication.delta, "Feature task integration diff"),
            (&proof.verification, "Feature task verification"),
        ] {
            let id = tx.new_id(IdKind::Link);
            tx.sessions().insert_link(&StoredLink {
                id,
                project_id: task.project_id.clone(),
                root_id: root.clone(),
                kind: "evidence".into(),
                target_kind: "artifact_version".into(),
                target_id: artifact.version_id.as_str().into(),
                parent_id: None,
                supersedes_id: None,
                actor: owner.clone(),
                event_id: event.0.clone(),
                sequence,
                repository_id: Some(tree.repository_id.clone()),
                revision: Some(tree.head.hex.clone()),
                summary: summary.into(),
                body: json!({"artifact":artifact}),
                created_at_ms: now,
            })?;
        }
    }
    if task.revision != proof.expected_task_revision.get()
        || task.parent_feature_id.as_ref() != Some(&tree.work_id)
    {
        return Ok(());
    }
    for repo in tx.work().work_repositories(&task.id)? {
        let Some(observed) = registered_publication(tx, &task.id, &repo)? else {
            if crate::services::reviews::recorded_no_changes(
                tx,
                &RootId(tree.work_id.clone()),
                &task.id,
                &proof.review_id,
                &repo,
            )? {
                continue;
            }
            return Ok(());
        };
        let source = serde_json::from_value::<Id>(observed.payload["taskWorktreeId"].clone())
            .map_err(|_| StorageError::ScopeDenied)?;
        if observed.payload["featureId"] != json!(tree.work_id)
            || observed.payload["reviewId"] != json!(proof.review_id)
            || tx.git().git_worktree(&source)?.is_none_or(|source| {
                source.removed
                    || source.work_id != task.id
                    || source.repository_id != repo
                    || source.pending_operation_id.is_some()
                    || observed.payload["taskHead"] != json!(source.head)
            })
        {
            return Ok(());
        }
    }
    crate::services::work::items::finish_integrated_task(tx, ctx, &task)
}

/// A workflow move or evidence edit cannot make a feature code task eligible ahead of the
/// native Git publication. Keep the manual workflow status and wait for the Git evidence.
pub(crate) fn waits_for_publication(
    tx: &mut dyn WriteTransaction,
    task: &StoredWork,
) -> Result<bool, StorageError> {
    let Some(feature) = &task.parent_feature_id else {
        return Ok(false);
    };
    let repos = tx.work().work_repositories(&task.id)?;
    let mut awaiting = Vec::new();
    let mut reviews = Vec::new();
    for repo in &repos {
        if !uses_local_integration(tx, feature, repo)? {
            continue;
        }
        if let Some(publication) = registered_publication(tx, &task.id, repo)?
            && publication.payload["featureId"] == json!(feature)
        {
            let review = serde_json::from_value::<Id>(publication.payload["reviewId"].clone())
                .map_err(|_| StorageError::ScopeDenied)?;
            if !reviews.contains(&review) {
                reviews.push(review);
            }
        } else {
            awaiting.push(repo);
        }
    }
    for repo in awaiting {
        let mut unchanged = false;
        for review in &reviews {
            if crate::services::reviews::recorded_no_changes(
                tx,
                &RootId(feature.clone()),
                &task.id,
                review,
                repo,
            )? {
                unchanged = true;
                break;
            }
        }
        if !unchanged {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn feature_head_digest(
    tx: &mut dyn WriteTransaction,
    feature: &Id,
) -> Result<Option<String>, StorageError> {
    let repositories = tx.work().work_repositories(feature)?;
    let mut heads = std::collections::BTreeMap::new();
    let mut after = None;
    loop {
        let page = tx.git().git_worktrees(feature, after.as_ref(), MAX_PAGE)?;
        for tree in &page {
            if tree.removed
                || !repositories.contains(&tree.repository_id)
                || !tree.branch.starts_with("refs/heads/feature/")
            {
                continue;
            }
            if !observed_published_head(tx, tree)? {
                return Ok(None);
            }
            let registration = tx
                .projects()
                .repository(&tree.repository_id)?
                .ok_or(StorageError::ScopeDenied)?;
            if heads.insert(tree.repository_id.as_str().to_owned(),
                json!({"branch":tree.branch,"head":tree.head,"registration":registration.revision})).is_some() {
                return Ok(None);
            }
        }
        if page.len() < usize::from(MAX_PAGE) {
            break;
        }
        after = page.last().map(|tree| tree.id.clone());
    }
    if heads.is_empty() {
        return Ok(None);
    }
    crate::fingerprint::canonical_digest(&json!({"repositories":repositories,"heads":heads}))
        .map(Some)
        .map_err(|_| StorageError::InvalidRecord("feature acceptance Git heads".into()))
}

pub(super) fn final_pr_coverage(
    tx: &mut dyn WriteTransaction,
    feature: &StoredWork,
    tree: &StoredGitWorktree,
) -> Result<Vec<(Id, i64, GitOid)>, Failure> {
    use crate::services::work::{
        model::{WorkBrief, active_of, feature_criteria, verified},
        workflow::{CLOSED, semantic_of},
    };

    let brief = WorkBrief::of(feature)?;
    let plan = tx.work().feature_plan(&feature.id)?;
    let active = active_of(plan.as_ref().and_then(|plan| plan.approved.as_ref()));
    let (criteria, tests) = feature_criteria(active.as_ref(), &brief);
    let acceptance: Vec<_> = criteria.into_iter().chain(tests).collect();
    let heads = feature_head_digest(tx, &feature.id)?;
    if !verified(&acceptance)
        || heads.as_ref().is_none_or(|heads| {
            acceptance.iter().any(|criterion| {
                brief.acceptance_git_heads.get(criterion.id.as_str()) != Some(heads)
            })
        })
    {
        return Err(refuse(
            DomainErrorCode::ApprovalRequired,
            "integrated_feature_acceptance",
            &feature.id,
        ));
    }
    let statuses = tx.work().statuses(&feature.project_id)?;
    let filter = WorkFilter {
        project: feature.project_id.clone(),
        selection: WorkSelection::FeatureTasks(feature.id.clone()),
        status_ids: vec![],
        repository_ids: vec![],
        text: None,
    };
    let mut covered = Vec::new();
    let mut after = None;
    loop {
        let page = tx.work().works(&filter, after.as_ref(), MAX_PAGE)?;
        for task in &page {
            if task
                .status_id
                .as_ref()
                .and_then(|id| semantic_of(&statuses, id))
                == Some(CLOSED)
            {
                continue;
            }
            if task.completion.as_deref() != Some(VERIFIED) {
                return Err(refuse(
                    DomainErrorCode::ApprovalRequired,
                    "integrated_task_acceptance",
                    &task.id,
                ));
            }
            let repositories = tx.work().work_repositories(&task.id)?;
            if !repositories.contains(&tree.repository_id) {
                continue;
            }
            if let Some(publication) = registered_publication(tx, &task.id, &tree.repository_id)? {
                if publication.payload["featureId"] != json!(feature.id)
                    || publication.payload["branch"] != json!(tree.branch)
                {
                    return Err(refuse(
                        DomainErrorCode::ApprovalRequired,
                        "feature_task_publication",
                        &task.id,
                    ));
                }
                let head =
                    serde_json::from_value::<GitOid>(publication.payload["candidate"].clone())
                        .map_err(|_| StorageError::ScopeDenied)?;
                if head.format != tree.head.format {
                    return Err(StorageError::ScopeDenied.into());
                }
                covered.push((task.id.clone(), task.revision, head));
                continue;
            }
            let mut unchanged = false;
            for other in repositories
                .iter()
                .filter(|repo| *repo != &tree.repository_id)
            {
                let Some(publication) = registered_publication(tx, &task.id, other)? else {
                    continue;
                };
                let review = serde_json::from_value::<Id>(publication.payload["reviewId"].clone())
                    .map_err(|_| StorageError::ScopeDenied)?;
                if crate::services::reviews::recorded_no_changes(
                    tx,
                    &RootId(feature.id.clone()),
                    &task.id,
                    &review,
                    &tree.repository_id,
                )? {
                    unchanged = true;
                    break;
                }
            }
            if !unchanged {
                return Err(refuse(
                    DomainErrorCode::ApprovalRequired,
                    "feature_task_publication",
                    &task.id,
                ));
            }
        }
        if page.len() < usize::from(MAX_PAGE) {
            break;
        }
        after = page.last().map(WorkPosition::of);
    }
    covered.sort_unstable_by(|(a, _, _), (b, _, _)| a.as_str().cmp(b.as_str()));
    Ok(covered)
}
