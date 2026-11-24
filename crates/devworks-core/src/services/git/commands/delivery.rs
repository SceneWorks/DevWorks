//! Records the exact final PRs and the single outstanding GitHub merge intent for a work root.
use super::*;
use crate::protocol::data::{
    DeliveryBarrier, DeliveryKind, DeliverySnapshot, MergeObservation, RepositoryDelivery,
    RepositoryMerge,
};
use crate::services::git::github::{Repository, delivery::QueueAnchor};

const DELIVERY_AGGREGATE: &str = "git_delivery";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Delivery {
    pub barrier: DeliveryBarrier,
    pub project: ProjectId,
    revision: i64,
    #[serde(default)]
    pub final_operation: Option<OperationId>,
    #[serde(default)]
    pub cleanup_stop_requested: bool,
    pub repositories: Vec<DeliveryRepository>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct DeliveryRepository {
    pub repo: Id,
    pub registration_revision: i64,
    pub worktree: Id,
    pub branch: String,
    pub target: Repository,
    pub number: u64,
    pub intent: Option<OperationId>,
    pub started: bool,
    pub method_witness: bool,
    #[serde(default)]
    pub confirmed_method_commit: Option<String>,
    pub merge_queue: Option<bool>,
    #[serde(default)]
    pub queue_anchor: Option<QueueAnchor>,
    #[serde(default)]
    pub queue_removal: Option<String>,
    pub ticket: Option<Id>,
    pub observation: MergeObservation,
    pub merge_commit: Option<GitOid>,
}

impl Delivery {
    pub(super) fn repository(&self, id: &Id) -> Result<&DeliveryRepository, StorageError> {
        self.repositories
            .iter()
            .find(|row| &row.repo == id)
            .ok_or(StorageError::ScopeDenied)
    }

    pub(super) fn entry(&self, id: &Id) -> Result<&RepositoryDelivery, StorageError> {
        self.barrier
            .repositories
            .iter()
            .find(|row| &row.repository_id == id)
            .ok_or(StorageError::ScopeDenied)
    }

    pub(super) fn update_repository(
        &mut self,
        id: &Id,
    ) -> Result<&mut DeliveryRepository, StorageError> {
        self.repositories
            .iter_mut()
            .find(|row| &row.repo == id)
            .ok_or(StorageError::ScopeDenied)
    }

    pub(super) fn all_merged(&self) -> bool {
        !self.repositories.is_empty()
            && self
                .repositories
                .iter()
                .all(|row| row.merge_commit.is_some())
    }

    pub(super) fn view(&self) -> Result<DeliverySnapshot, StorageError> {
        Ok(DeliverySnapshot {
            delivery_id: self.barrier.delivery_id.clone(),
            revision: self.barrier.revision.clone(),
            repositories: self
                .repositories
                .iter()
                .map(|row| {
                    let entry = self.entry(&row.repo)?;
                    Ok(RepositoryMerge {
                        request_ticket_id: row.ticket.clone(),
                        evidence: entry.evidence.clone(),
                        repository_id: row.repo.clone(),
                        expected_head: entry.expected_head.clone(),
                        observation: row.observation.clone(),
                        merge_commit: row.merge_commit.clone(),
                    })
                })
                .collect::<Result<_, StorageError>>()?,
            feature_complete: self.final_operation.is_some(),
            cleanup_pending: self.final_operation.is_some(),
            release_pending: false,
            deployment_pending: false,
        })
    }
}

pub(super) fn read_delivery(
    history: &dyn HistoryRead,
    root: &RootId,
) -> Result<Option<Delivery>, StorageError> {
    let filter = EventFilter::Aggregate {
        kind: DELIVERY_AGGREGATE.into(),
        id: root.0.as_str().into(),
    };
    let mut after = 0;
    let mut last = None;
    loop {
        let page = history.page(&filter, after, MAX_PAGE)?;
        if let Some(event) = page.events.into_iter().last() {
            last = Some(event);
        }
        if !page.has_more {
            break;
        }
        after = page.next_after;
    }
    last.map(|last| {
        let delivery: Delivery = serde_json::from_value(last.payload)
            .map_err(|_| StorageError::InvalidRecord("final delivery record".into()))?;
        if &delivery.barrier.root_id != root || delivery.revision != last.aggregate_revision {
            return Err(StorageError::InvalidRecord(
                "final delivery identity".into(),
            ));
        }
        Ok(delivery)
    })
    .transpose()
}

pub(super) fn write_delivery(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    operation: &OperationId,
    kind: &str,
    delivery: &mut Delivery,
) -> Result<(), StorageError> {
    delivery.revision = delivery
        .revision
        .checked_add(1)
        .ok_or_else(|| StorageError::InvalidRecord("final delivery revision exhausted".into()))?;
    let payload = serde_json::to_value(&*delivery)
        .map_err(|_| StorageError::InvalidRecord("final delivery record".into()))?;
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event,
        actor: actor.clone(),
        project_id: Some(delivery.project.clone()),
        root_id: Some(delivery.barrier.root_id.clone()),
        operation_id: Some(operation.clone()),
        aggregate_kind: DELIVERY_AGGREGATE.into(),
        aggregate_id: delivery.barrier.root_id.0.as_str().into(),
        aggregate_revision: delivery.revision,
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}

fn same_value<T: Serialize>(a: &T, b: &T) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

fn predecessors<'a>(barrier: &'a DeliveryBarrier, repo: &Id) -> Vec<&'a str> {
    let mut edges: Vec<_> = barrier
        .dependencies
        .iter()
        .filter(|edge| &edge.after == repo)
        .map(|edge| edge.before.as_str())
        .collect();
    edges.sort_unstable();
    edges.dedup();
    edges
}

fn check_refresh(delivery: &Delivery, candidate: &DeliveryBarrier) -> Result<(), Failure> {
    let invalid = || {
        refuse(
            DomainErrorCode::SourceChanged,
            "delivery_barrier",
            &candidate.delivery_id,
        )
    };
    let previous = &delivery.barrier;
    if candidate.root_id != previous.root_id
        || candidate.approved_scope_digest != previous.approved_scope_digest
        || !same_value(&candidate.kind, &previous.kind)
        || candidate.revision.get() != previous.revision.get() + 1
        || previous.repositories.len() != candidate.repositories.len()
        || previous.repositories.iter().any(|entry| {
            !candidate
                .repositories
                .iter()
                .any(|other| other.repository_id == entry.repository_id)
        })
    {
        return Err(invalid());
    }
    for row in &delivery.repositories {
        if row.intent.is_some() || row.merge_commit.is_some() {
            let old = delivery.entry(&row.repo)?;
            let current = candidate
                .repositories
                .iter()
                .find(|entry| entry.repository_id == row.repo)
                .ok_or_else(invalid)?;
            if !same_value(old, current)
                || predecessors(previous, &row.repo) != predecessors(candidate, &row.repo)
            {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

fn bind_repository(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    work: &StoredWork,
    entry: &RepositoryDelivery,
) -> Result<DeliveryRepository, Failure> {
    let invalid = || {
        refuse(
            DomainErrorCode::SourceChanged,
            "delivery_pull_request",
            &entry.repository_id,
        )
    };
    let registration = tx
        .projects()
        .repository(&entry.repository_id)?
        .filter(|repo| repo.lifecycle == ACTIVE)
        .ok_or_else(invalid)?;
    if !tx
        .projects()
        .members(&work.project_id)?
        .contains(&entry.repository_id)
    {
        return Err(invalid());
    }
    let target = registration
        .remote_url
        .as_deref()
        .and_then(Repository::from_remote)
        .ok_or_else(invalid)?;
    let link = tx
        .sessions()
        .link(&entry.pull_request_id)?
        .filter(|link| {
            link.root_id.0 == work.id
                && link.project_id == work.project_id
                && link.repository_id.as_ref() == Some(&entry.repository_id)
                && link.target_kind == "pull_request"
        })
        .ok_or_else(invalid)?;
    let stem = format!("https://github.com/{}/{}/pull/", target.owner, target.name);
    let number = link
        .target_id
        .strip_prefix(&stem)
        .and_then(|number| number.parse::<u64>().ok())
        .filter(|number| *number > 0)
        .ok_or_else(invalid)?;
    if link.target_id != format!("{stem}{number}") {
        return Err(invalid());
    }
    let tree =
        serde_json::from_value::<Id>(link.body["worktreeId"].clone()).map_err(|_| invalid())?;
    let tree = tx
        .git()
        .git_worktree(&tree)?
        .filter(|tree| {
            tree.work_id == work.id
                && !tree.removed
                && tree.repository_id == registration.id
                && tree.branch == link.body["branch"].as_str().unwrap_or_default()
                && tree.head == entry.expected_head
        })
        .ok_or_else(invalid)?;
    if work.kind == FEATURE && !tree.branch.starts_with("refs/heads/feature/") {
        return Err(invalid());
    }
    crate::services::work::check_artifact(tx, ctx, &work.project_id, &entry.evidence)?;
    Ok(DeliveryRepository {
        repo: registration.id,
        registration_revision: registration.revision,
        worktree: tree.id,
        branch: tree.branch,
        target,
        number,
        intent: None,
        started: false,
        method_witness: false,
        confirmed_method_commit: None,
        merge_queue: None,
        queue_anchor: None,
        queue_removal: None,
        ticket: None,
        observation: MergeObservation::NotMerged,
        merge_commit: None,
    })
}

pub(super) fn reserve_delivery(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    operation: &OperationId,
    input: &GitCommand,
) -> Result<(Delivery, StoredWork), Failure> {
    let GitCommand::Integrate {
        delivery_id,
        expected_barrier_revision,
        repository_id,
        expected_head,
        barrier,
    } = input
    else {
        unreachable!("selected final integration")
    };
    let root = ctx.root_id().ok_or(StorageError::ScopeDenied)?;
    let current = read_delivery(tx.history(), root)?;
    if current
        .as_ref()
        .is_some_and(|state| &state.barrier.delivery_id != delivery_id)
    {
        return Err(refuse(
            DomainErrorCode::Conflict,
            "delivery_identity",
            delivery_id,
        ));
    }
    let Some(candidate) = barrier
        .as_ref()
        .or_else(|| current.as_ref().map(|state| &state.barrier))
    else {
        return Err(refuse(
            DomainErrorCode::NotFound,
            "delivery_barrier",
            delivery_id,
        ));
    };
    let invalid = || {
        refuse(
            DomainErrorCode::SourceChanged,
            "delivery_barrier",
            delivery_id,
        )
    };
    if &candidate.revision != expected_barrier_revision || ctx.root_id() != Some(&candidate.root_id)
    {
        return Err(invalid());
    }
    let work = crate::services::work::visible_work(tx.work(), ctx, &candidate.root_id.0)?;
    crate::services::work::open_project(tx, ctx, &work.project_id)?;
    if ctx.project_id() != Some(&work.project_id) || !GitService::pr_manager(tx, ctx, &work)? {
        return Err(StorageError::ScopeDenied.into());
    }
    match candidate.kind {
        DeliveryKind::Feature if work.kind == FEATURE => {
            let active = tx
                .work()
                .feature_plan(&work.id)?
                .and_then(|row| crate::services::work::model::active_of(row.approved.as_ref()))
                .ok_or_else(invalid)?;
            if active.digest != candidate.approved_scope_digest.0 {
                return Err(invalid());
            }
        }
        DeliveryKind::StandaloneIssue
            if work.parent_feature_id.is_none() && matches!(work.kind.as_str(), TASK | "issue") => {
        }
        _ => return Err(invalid()),
    }
    let selected = tx.work().work_repositories(&work.id)?;
    if selected.len() != candidate.repositories.len()
        || candidate.repositories.iter().any(|entry| {
            !selected.contains(&entry.repository_id)
                || &entry.policy_revision != ctx.policy_revision()
        })
    {
        return Err(invalid());
    }
    let mut proposed = match current.as_ref() {
        Some(current) if current.project != work.project_id => return Err(invalid()),
        Some(current) if same_value(&current.barrier, candidate) => current.clone(),
        Some(current) => {
            check_refresh(current, candidate)?;
            let mut next = current.clone();
            next.barrier = candidate.clone();
            for row in &mut next.repositories {
                if row.intent.is_none() && row.merge_commit.is_none() {
                    let entry = candidate
                        .repositories
                        .iter()
                        .find(|entry| entry.repository_id == row.repo)
                        .ok_or_else(invalid)?;
                    *row = bind_repository(tx, ctx, &work, entry)?;
                }
            }
            next
        }
        None if candidate.revision.get() == 1 => Delivery {
            barrier: candidate.clone(),
            project: work.project_id.clone(),
            revision: 0,
            final_operation: None,
            cleanup_stop_requested: false,
            repositories: candidate
                .repositories
                .iter()
                .map(|entry| bind_repository(tx, ctx, &work, entry))
                .collect::<Result<_, _>>()?,
        },
        None => return Err(invalid()),
    };
    if proposed.entry(repository_id)?.expected_head != *expected_head
        || predecessors(&proposed.barrier, repository_id)
            .into_iter()
            .any(|before| {
                !proposed
                    .repositories
                    .iter()
                    .any(|row| row.repo.as_str() == before && row.merge_commit.is_some())
            })
        || proposed.repositories.iter().any(|row| row.intent.is_some())
    {
        return Err(refuse(
            DomainErrorCode::Conflict,
            "delivery_dependencies",
            repository_id,
        ));
    }
    let row = proposed.update_repository(repository_id)?;
    if row.merge_commit.is_some() {
        return Err(refuse(
            DomainErrorCode::Conflict,
            "delivery_merged",
            repository_id,
        ));
    }
    row.intent = Some(operation.clone());
    row.started = false;
    row.method_witness = false;
    row.confirmed_method_commit = None;
    row.merge_queue = None;
    row.queue_anchor = None;
    row.queue_removal = None;
    row.ticket = None;
    row.observation = MergeObservation::NotMerged;
    Ok((proposed, work))
}

pub(super) fn record_delivery_intent(
    tx: &mut dyn WriteTransaction,
    actor: &ActorId,
    operation: &OperationId,
    delivery: &mut Delivery,
) -> Result<(), StorageError> {
    write_delivery(tx, actor, operation, "FinalMergeReserved", delivery)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct CleanupProof {
    pub root: RootId,
    pub final_head: GitOid,
    pub integrated_candidate: GitOid,
    pub merge_commit: GitOid,
}

pub(super) fn cleanup_proof(
    tx: &mut dyn WriteTransaction,
    tree: &StoredGitWorktree,
    work: &StoredWork,
) -> Result<Option<CleanupProof>, Failure> {
    let root = RootId(
        work.parent_feature_id
            .clone()
            .unwrap_or_else(|| work.id.clone()),
    );
    let state = read_delivery(tx.history(), &root)?;
    if state.is_none()
        && !tree.branch.starts_with("refs/heads/feature/")
        && (work.parent_feature_id.is_none()
            || !feature_delivery::uses_local_integration(tx, &root.0, &tree.repository_id)?)
    {
        return Ok(None);
    }
    let invalid = || {
        refuse(
            DomainErrorCode::ApprovalRequired,
            "verified_delivery_cleanup",
            &tree.id,
        )
    };
    let state = state
        .filter(|state| state.all_merged() && state.final_operation.is_some())
        .ok_or_else(invalid)?;
    let row = state.repository(&tree.repository_id)?;
    let entry = state.entry(&tree.repository_id)?;
    if tx
        .projects()
        .repository(&tree.repository_id)?
        .is_none_or(|repo| repo.revision != row.registration_revision || repo.lifecycle != ACTIVE)
    {
        return Err(invalid());
    }
    let integrated = if row.worktree == tree.id && entry.expected_head == tree.head {
        tree.head.clone()
    } else {
        let historical =
            feature_delivery::registered_publication(tx, &work.id, &tree.repository_id)?
                .ok_or_else(invalid)?;
        if historical.payload["featureId"] != json!(root.0)
            || historical.payload["taskWorktreeId"] != json!(tree.id)
            || historical.payload["taskHead"] != json!(tree.head)
        {
            return Err(invalid());
        }
        let integrated: GitOid = serde_json::from_value(historical.payload["candidate"].clone())
            .map_err(|_| invalid())?;
        integrated.validate().map_err(|_| invalid())?;
        integrated
    };
    Ok(Some(CleanupProof {
        root,
        final_head: entry.expected_head.clone(),
        integrated_candidate: integrated,
        merge_commit: row.merge_commit.clone().ok_or_else(invalid)?,
    }))
}

pub(super) fn release_refused_delivery(
    tx: &mut dyn WriteTransaction,
    job: &Job,
    operation: &OperationId,
) -> Result<(), StorageError> {
    let mut delivery = read_delivery(tx.history(), &RootId(job.tree.work_id.clone()))?
        .ok_or(StorageError::ScopeDenied)?;
    let row = delivery.update_repository(&job.tree.repository_id)?;
    if row.intent.as_ref() != Some(operation) || row.started {
        return Err(StorageError::ScopeDenied);
    }
    row.intent = None;
    write_delivery(
        tx,
        &job.actor,
        operation,
        "FinalMergeRefused",
        &mut delivery,
    )
}

// Reuse the exact-head review decisions while independently observing the required GitHub
// checks and current branch policy for every PR that can still change.
impl GitService {
    fn delivery_state(&self, job: &Job, operation: &OperationId) -> Result<Delivery, StorageError> {
        let root = RootId(job.tree.work_id.clone());
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), |snapshot| {
            let state =
                read_delivery(snapshot.history(), &root)?.ok_or(StorageError::ScopeDenied)?;
            let repository = state.repository(&job.tree.repository_id)?;
            if repository.intent.as_ref() != Some(operation)
                || repository.worktree != job.tree.id
                || state.project != job.project
            {
                return Err(StorageError::ScopeDenied);
            }
            Ok(state)
        })
    }

    fn delivery_authority(
        &self,
        ctx: &InvocationContext,
        delivery: &Delivery,
    ) -> Result<(), Failure> {
        use crate::operations::reviews::ReadinessInput;
        let reviewed = delivery
            .repositories
            .iter()
            .filter(|row| row.merge_commit.is_some())
            .map(|row| row.repo.clone())
            .collect::<Vec<_>>();
        let checks = |snapshot: &dyn ReadSnapshot| -> Result<Result<(), Failure>, StorageError> {
            for row in delivery
                .repositories
                .iter()
                .filter(|row| row.merge_commit.is_none())
            {
                let live = snapshot.projects().repository(&row.repo)?;
                if live.is_none_or(|repo| {
                    repo.lifecycle != ACTIVE
                        || repo.revision != row.registration_revision
                        || repo
                            .remote_url
                            .as_deref()
                            .and_then(Repository::from_remote)
                            .as_ref()
                            != Some(&row.target)
                }) {
                    return Ok(Err(refuse(
                        DomainErrorCode::SourceChanged,
                        "delivery_repository",
                        &row.repo,
                    )));
                }
                let setting = crate::settings::service::delivery_auto_merge(
                    snapshot.configuration(),
                    &delivery.project,
                    &row.repo,
                )
                .map_err(Failure::into_storage)?;
                if setting.value.value.0.as_bool() != Some(true) {
                    return Ok(Err(refuse(
                        DomainErrorCode::ApprovalRequired,
                        "auto_merge_disabled",
                        &row.repo,
                    )));
                }
            }
            let input = ReadinessInput {
                root_id: delivery.barrier.root_id.clone(),
                barrier: Some(delivery.barrier.clone()),
                policy_revision: ctx.policy_revision().clone(),
                local_target: None,
            };
            let checked = crate::services::reviews::ReviewService::readiness_with_merged(
                snapshot, ctx, &input, &reviewed,
            );
            Ok(match checked {
                Ok((ready, _)) if ready.ready => Ok(()),
                Ok(_) => Err(refuse(
                    DomainErrorCode::Conflict,
                    "exact_head_review",
                    &delivery.barrier.delivery_id,
                )),
                Err(error) => Err(error),
            })
        };
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), checks)??;
        Ok(())
    }

    fn verified_merges_reach_main(&self, delivery: &Delivery) -> Result<(), Failure> {
        for row in &delivery.repositories {
            let Some(commit) = &row.merge_commit else {
                continue;
            };
            let observed = self.config.github.as_ref().and_then(|client| {
                let main = client
                    .branch_head(self.store.as_ref(), &row.target, "main")
                    .ok()?;
                client
                    .commit_reaches_main(self.store.as_ref(), &row.target, &commit.hex, &main)
                    .ok()
            });
            if observed != Some(true) {
                return Err(refuse(
                    DomainErrorCode::SourceChanged,
                    "verified_merge_on_main",
                    &row.repo,
                ));
            }
        }
        Ok(())
    }

    fn prove_delivery_gates(
        &self,
        ctx: &InvocationContext,
        delivery: &Delivery,
        selected: &Id,
    ) -> Result<bool, Failure> {
        use crate::services::git::github::delivery::method_name;
        let unavailable = |id: &Id| refuse(DomainErrorCode::ExternalFailure, "github_delivery", id);
        let client = self
            .config
            .github
            .as_ref()
            .ok_or_else(|| unavailable(selected))?;
        let observation = self
            .delivery_github
            .as_ref()
            .ok_or_else(|| unavailable(selected))?;
        let mut merge_policy = None;
        self.delivery_authority(ctx, delivery)?;
        self.verified_merges_reach_main(delivery)?;
        for row in delivery
            .repositories
            .iter()
            .filter(|row| row.merge_commit.is_none())
        {
            let entry = delivery.entry(&row.repo)?;
            let pull = client
                .delivery_pull(self.store.as_ref(), &row.target, row.number)
                .map_err(|_| {
                    refuse(
                        DomainErrorCode::ExternalFailure,
                        "github_delivery_pull",
                        &row.repo,
                    )
                })?;
            let main = client
                .branch_head(self.store.as_ref(), &row.target, "main")
                .map_err(|_| {
                    refuse(
                        DomainErrorCode::ExternalFailure,
                        "github_delivery_main",
                        &row.repo,
                    )
                })?;
            if !pull.open
                || pull.draft
                || pull.mergeable != Some(true)
                || pull.head != entry.expected_head.hex
                || pull.base != entry.observed_base.hex
                || main != pull.base
                || pull.base_ref != "main"
                || row.branch.strip_prefix("refs/heads/") != Some(pull.head_ref.as_str())
            {
                return Err(refuse(
                    DomainErrorCode::SourceChanged,
                    "delivery_pull_request",
                    &row.repo,
                ));
            }
            if client.single_final_pull(
                self.store.as_ref(),
                &row.target,
                row.number,
                &entry.expected_head.hex,
            ) != Ok(true)
            {
                return Err(refuse(
                    DomainErrorCode::Conflict,
                    "github_single_final_pull",
                    &row.repo,
                ));
            }
            if !client
                .can_merge(self.store.as_ref(), &row.target, &entry.merge_method)
                .map_err(|_| {
                    refuse(
                        DomainErrorCode::ExternalFailure,
                        "github_delivery_permission",
                        &row.repo,
                    )
                })?
            {
                return Err(refuse(
                    DomainErrorCode::PermissionDenied,
                    "github_delivery_permission",
                    &row.repo,
                ));
            }
            let rules = observation.delivery_review_rules(&delivery.project, &row.repo)?;
            if (rules.fresh_review || rules.required_approvals)
                && pull.mergeable_state.as_deref() != Some("clean")
            {
                return Err(refuse(
                    DomainErrorCode::ApprovalRequired,
                    "github_required_review",
                    &row.repo,
                ));
            }
            if !matches!(
                pull.mergeable_state.as_deref(),
                Some("clean" | "has_hooks" | "unstable")
            ) || (rules.requires_clean && pull.mergeable_state.as_deref() != Some("clean"))
                || (rules.requires_linear_history
                    && matches!(
                        entry.merge_method,
                        crate::protocol::data::MergeMethod::MergeCommit
                    ))
            {
                return Err(refuse(
                    DomainErrorCode::Conflict,
                    "github_branch_protection",
                    &row.repo,
                ));
            }
            if rules
                .merge_queue_method
                .as_deref()
                .is_some_and(|method| method != method_name(&entry.merge_method))
            {
                return Err(refuse(
                    DomainErrorCode::Conflict,
                    "github_merge_queue_method",
                    &row.repo,
                ));
            }
            if !client
                .required_checks_green(
                    self.store.as_ref(),
                    &row.target,
                    &entry.expected_head.hex,
                    &rules.required_checks,
                )
                .map_err(|_| {
                    refuse(
                        DomainErrorCode::ExternalFailure,
                        "github_delivery_checks",
                        &row.repo,
                    )
                })?
            {
                return Err(refuse(
                    DomainErrorCode::Conflict,
                    "github_required_checks",
                    &row.repo,
                ));
            }
            if selected == &row.repo {
                merge_policy = Some(rules.merge_queue_method.is_some());
            }
        }
        self.delivery_authority(ctx, delivery)?;
        merge_policy.ok_or_else(|| unavailable(selected))
    }

    fn update_delivery(
        &self,
        job: &Job,
        operation: &OperationId,
        kind: &'static str,
        change: impl FnOnce(&mut DeliveryRepository) -> Result<(), StorageError> + Send + 'static,
    ) -> Result<Delivery, StorageError> {
        let (j, op) = (job.clone(), operation.clone());
        run_write(self.store.as_ref(), WriteOptions::settlement(), move |tx| {
            let mut delivery = read_delivery(tx.history(), &RootId(j.tree.work_id.clone()))?
                .ok_or(StorageError::ScopeDenied)?;
            let row = delivery.update_repository(&j.tree.repository_id)?;
            if row.intent.as_ref() != Some(&op) {
                return Err(StorageError::ScopeDenied);
            }
            change(row)?;
            write_delivery(tx, &j.actor, &op, kind, &mut delivery)?;
            Ok(delivery)
        })
    }

    /// Durable move to submitting precedes the only mutating transport call. A crash across
    /// that boundary can leave a hold, and cannot authorize a repeat.
    pub(super) fn execute_integrate(
        &self,
        job: &Job,
        operation: &OperationRecord,
        ctx: &InvocationContext,
    ) -> EffectOutcome {
        use crate::services::git::github::{
            Fault,
            delivery::{MergeRequest, MergeTarget},
        };
        let delivery = match self.delivery_state(job, &operation.id) {
            Ok(delivery) => delivery,
            Err(error) => return storage_failure(error),
        };
        let Ok(row) = delivery.repository(&job.tree.repository_id) else {
            return denied("delivery reservation changed");
        };
        if row.started {
            return self.observe_integrate(job, operation);
        }
        let use_queue = match self.prove_delivery_gates(ctx, &delivery, &row.repo) {
            Ok(queue) => queue,
            Err(Failure::Refused(fault)) => return failed(fault.code, fault.target_kind),
            Err(Failure::Storage(_)) => {
                return denied("current delivery authority could not be verified");
            }
        };
        if self.git_stopped(job) {
            return denied("work or authority changed before final merge");
        }
        let Some(client) = self.config.github.as_ref() else {
            return denied("GitHub client unavailable");
        };
        let Ok(entry) = delivery.entry(&row.repo) else {
            return denied("delivery candidate changed");
        };
        let queue_anchor = if use_queue {
            match client.merge_queue_anchor(
                self.store.as_ref(),
                &row.target,
                row.number,
                &entry.expected_head.hex,
            ) {
                Ok(anchor) => Some(anchor),
                Err(_) => return denied("could not bind the exact merge queue before submission"),
            }
        } else {
            None
        };
        let target = MergeTarget {
            expected_head: &entry.expected_head.hex,
            method: &entry.merge_method,
            use_queue,
        };
        if let Err(error) =
            self.update_delivery(job, &operation.id, "FinalMergeSubmitting", move |row| {
                if row.started {
                    return Err(StorageError::ScopeDenied);
                }
                row.started = true;
                row.merge_queue = Some(use_queue);
                row.queue_anchor = queue_anchor;
                row.observation = MergeObservation::OutcomeUnknown;
                Ok(())
            })
        {
            return storage_failure(error);
        }
        if self.git_stopped(job) || self.delivery_authority(ctx, &delivery).is_err() {
            if let Err(error) =
                self.update_delivery(job, &operation.id, "FinalMergeNotSubmitted", |row| {
                    row.started = false;
                    row.observation = MergeObservation::NotMerged;
                    Ok(())
                })
            {
                return storage_failure(error);
            }
            return denied("delivery authority changed before submission");
        }
        match client.submit_merge(self.store.as_ref(), &row.target, row.number, &target) {
            Ok(MergeRequest::Accepted(ticket)) => {
                let witness = Id::new(ticket).ok();
                if let Err(error) =
                    self.update_delivery(job, &operation.id, "FinalMergeAccepted", move |row| {
                        row.method_witness = witness.is_some();
                        row.ticket = witness;
                        row.observation = MergeObservation::Accepted;
                        Ok(())
                    })
                {
                    return storage_failure(error);
                }
            }
            Ok(MergeRequest::Enqueued) => {
                if let Err(error) =
                    self.update_delivery(job, &operation.id, "FinalMergeQueued", |row| {
                        row.observation = MergeObservation::Queued;
                        Ok(())
                    })
                {
                    return storage_failure(error);
                }
            }
            Ok(MergeRequest::Refused) | Err(Fault::Rejected(400 | 403 | 422)) => {
                if let Err(error) =
                    self.update_delivery(job, &operation.id, "FinalMergeRefused", |row| {
                        row.started = false;
                        row.observation = MergeObservation::NotMerged;
                        Ok(())
                    })
                {
                    return storage_failure(error);
                }
                return denied("GitHub refused the exact merge request");
            }
            // GitHub's async 200 can report a competing merge already in place. Its SHA
            // alone does not attribute a direct squash or rebase to this request.
            _ => {}
        }
        self.observe_integrate(job, operation)
    }

    pub(super) fn observe_integrate(
        &self,
        job: &Job,
        operation: &OperationRecord,
    ) -> EffectOutcome {
        use crate::protocol::data::MergeMethod;
        use crate::services::git::github::delivery::{MergeRequest, MergeTarget};
        let unknown =
            || EffectOutcome::Unknown("final merge outcome, source or method is unverified".into());
        let Ok(delivery) = self.delivery_state(job, &operation.id) else {
            return unknown();
        };
        let (Ok(row), Ok(entry)) = (
            delivery.repository(&job.tree.repository_id),
            delivery.entry(&job.tree.repository_id),
        ) else {
            return unknown();
        };
        let Some(client) = self.config.github.as_ref() else {
            return unknown();
        };
        // This positive local witness means the adapter never crossed its submission boundary.
        if !row.started {
            return denied("the merge request did not reach submission");
        }
        let current = client.delivery_pull(self.store.as_ref(), &row.target, row.number);
        let Ok(pull) = current else {
            return unknown();
        };
        if pull.base_ref != "main"
            || row.branch.strip_prefix("refs/heads/") != Some(pull.head_ref.as_str())
        {
            return unknown();
        }
        let target = MergeTarget {
            expected_head: &entry.expected_head.hex,
            method: &entry.merge_method,
            use_queue: row.merge_queue.unwrap_or(false),
        };
        let ticket_outcome = row.ticket.as_ref().map(|ticket| {
            client.observe_merge_request(
                self.store.as_ref(),
                &row.target,
                row.number,
                ticket.as_str(),
                &target,
            )
        });
        if matches!(ticket_outcome, Some(Ok(MergeRequest::Refused))) && pull.merged.is_none() {
            if let Err(error) =
                self.update_delivery(job, &operation.id, "FinalMergeRefused", |row| {
                    row.started = false;
                    row.observation = MergeObservation::NotMerged;
                    Ok(())
                })
            {
                return storage_failure(error);
            }
            return denied("GitHub observed that the exact merge request failed");
        }
        let mut confirmed_method_commit = row.confirmed_method_commit.clone();
        if row.method_witness
            && !target.use_queue
            && let Some(Ok(MergeRequest::Merged(commit))) = &ticket_outcome
            && Some(commit) != confirmed_method_commit.as_ref()
        {
            if confirmed_method_commit.is_some() {
                return unknown();
            }
            confirmed_method_commit = Some(commit.clone());
            let evidence = confirmed_method_commit.clone();
            if let Err(error) =
                self.update_delivery(job, &operation.id, "FinalMergeMethodObserved", |row| {
                    row.confirmed_method_commit = evidence;
                    Ok(())
                })
            {
                return storage_failure(error);
            }
        }
        let queue = row.queue_anchor.as_ref().and_then(|anchor| {
            client
                .observe_merge_queue(
                    self.store.as_ref(),
                    &row.target,
                    row.number,
                    &entry.expected_head.hex,
                    anchor,
                )
                .ok()
        });
        if pull.merged.is_none()
            && let Some(removal) = queue.as_ref().and_then(|queue| queue.removal.as_ref())
            && (row.ticket.is_none() || matches!(ticket_outcome, Some(Ok(MergeRequest::Enqueued))))
        {
            let removal = removal.clone();
            if let Err(error) =
                self.update_delivery(job, &operation.id, "FinalMergeDequeued", |row| {
                    row.started = false;
                    row.observation = MergeObservation::NotMerged;
                    row.queue_removal = Some(removal);
                    Ok(())
                })
            {
                return storage_failure(error);
            }
            return denied("GitHub removed the exact request from the merge queue");
        }
        if pull.head != entry.expected_head.hex {
            return unknown();
        }
        let Some(merge) = pull.merged else {
            if (matches!(ticket_outcome, Some(Ok(MergeRequest::Enqueued)))
                || queue.as_ref().is_some_and(|queue| queue.active))
                && !matches!(row.observation, MergeObservation::Queued)
                && let Err(error) =
                    self.update_delivery(job, &operation.id, "FinalMergeQueued", |row| {
                        row.observation = MergeObservation::Queued;
                        Ok(())
                    })
            {
                return storage_failure(error);
            }
            return unknown();
        };
        let Ok(main) = client.branch_head(self.store.as_ref(), &row.target, "main") else {
            return unknown();
        };
        if client.commit_reaches_main(self.store.as_ref(), &row.target, &merge, &main) != Ok(true) {
            return unknown();
        }
        let method_matches = match entry.merge_method {
            MergeMethod::MergeCommit => client
                .commit_parents_on_github(self.store.as_ref(), &row.target, &merge)
                .is_ok_and(|parents| {
                    parents.len() == 2
                        && parents[1] == entry.expected_head.hex
                        && client.commit_reaches_main(
                            self.store.as_ref(),
                            &row.target,
                            &entry.observed_base.hex,
                            &parents[0],
                        ) == Ok(true)
                }),
            MergeMethod::Squash | MergeMethod::Rebase => {
                confirmed_method_commit.as_deref() == Some(merge.as_str())
                    || queue.as_ref().is_some_and(|queue| {
                        queue.merged_method.as_deref()
                            == Some(crate::services::git::github::delivery::method_name(
                                &entry.merge_method,
                            ))
                    })
            }
        };
        if !method_matches || self.verified_merges_reach_main(&delivery).is_err() {
            return unknown();
        }
        self.finish_integrate(job, &operation.id, oid(merge))
    }

    fn finish_integrate(&self, job: &Job, operation: &OperationId, merge: GitOid) -> EffectOutcome {
        let (j, op) = (job.clone(), operation.clone());
        let Some(ctx) = j.context(self, &op) else {
            return denied("invalid principal kind");
        };
        let result = run_write(self.store.as_ref(), WriteOptions::settlement(), move |tx| {
            let mut tree = tx
                .git()
                .git_worktree(&j.tree.id)?
                .ok_or(StorageError::ScopeDenied)?;
            if tree.last_operation_id.as_ref() == Some(&op) && tree.pending_operation_id.is_none() {
                return tree.last_result.ok_or(StorageError::ScopeDenied);
            }
            if tree != j.tree {
                return Err(StorageError::ScopeDenied);
            }
            let mut state = read_delivery(tx.history(), &RootId(j.tree.work_id.clone()))?
                .ok_or(StorageError::ScopeDenied)?;
            let row = state.update_repository(&j.tree.repository_id)?;
            if row.intent.as_ref() != Some(&op) {
                return Err(StorageError::ScopeDenied);
            }
            row.intent = None;
            row.merge_commit = Some(merge);
            row.observation = MergeObservation::Merged;
            if state.all_merged() {
                let root = tx
                    .work()
                    .work(&state.barrier.root_id.0)?
                    .ok_or(StorageError::ScopeDenied)?;
                let current = if root.kind == FEATURE {
                    tx.work()
                        .feature_plan(&root.id)?
                        .and_then(|plan| {
                            crate::services::work::model::active_of(plan.approved.as_ref())
                        })
                        .is_some_and(|plan| plan.digest == state.barrier.approved_scope_digest.0)
                } else {
                    root.revision == j.work_revision
                };
                if current {
                    state.final_operation = Some(op.clone());
                    crate::services::work::items::finish_delivered_work(tx, &ctx, &root)?;
                    super::auto_cleanup::record_cleanup_request(tx, &j, &op)?;
                }
            }
            write_delivery(tx, &j.actor, &op, "FinalMergeObserved", &mut state)?;
            let snapshot = state.view()?;
            let result = json!({"result":OperationResult::GitIntegrate{value:snapshot}});
            let prior = tree.revision;
            tree.revision += 1;
            tree.pending_operation_id = None;
            tree.last_operation_id = Some(op.clone());
            tree.last_result = Some(result.clone());
            tx.git().update_git_worktree(&tree, prior)?;
            Ok(result)
        });
        match result {
            Ok(value) => EffectOutcome::Completed(value),
            Err(error) => storage_failure(error),
        }
    }
}
