//! Final pull requests consume rounds per registered repository and PR, across head changes.
use super::commands::{LocalInput, persist_request, refused};
use super::policy::{ReviewStage, RoundAdmission, admit_round};
use super::{ReviewService, ReviewStateRecord, ReviewTarget, decode};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedCommand;
use crate::operations::reviews::RequestInput;
use crate::ports::storage::{ArtifactScope, FEATURE, StorageError, StoredLink, WriteTransaction};
use crate::ports::{IdKind, Preparation};
use crate::protocol::data::{Coverage, CoverageState, DeliveryBarrier, DeliveryKind, ReviewSource};
use crate::protocol::provider::Digest;
use crate::protocol::records::{
    ReviewObservationState, ReviewRecord, ReviewSourceEvidence, ReviewSourceKind, ReviewState,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, PositiveCounter};
use crate::protocol::{DomainErrorCode, Id, OperationId, ProjectId, RootId};
use crate::services::kernel::{Failure, counter, not_found, rev};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FinalRound {
    pub repository_id: Id,
    pub pull_id: Id,
    pub pull_number: u64,
    pub cycle: u8,
    pub settings_digest: String,
    pub bot_id: Option<u64>,
    pub requested_at_ms: i64,
    #[serde(default)]
    pub fresh_bot_after_ms: Option<i64>,
    pub deadline_ms: i64,
    pub head_digest: Digest,
    pub fallback_requested: bool,
    pub observed_ids: Vec<Id>,
    #[serde(default)]
    pub delivery_rules: Option<crate::services::github::code_review::DeliveryReviewRules>,
}

pub(super) fn review_lineage(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
    root: &RootId,
    repository: &Id,
    pull_id: &Id,
) -> Result<Vec<ReviewStateRecord>, Failure> {
    let mut states = Vec::new();
    let mut after = None;
    loop {
        let page = tx
            .git()
            .code_reviews(Some(project), Some(root), after.as_ref(), 128)?;
        for row in &page {
            let saved: ReviewStateRecord = decode(&row.body)?;
            if saved
                .final_rounds
                .iter()
                .any(|round| &round.repository_id == repository && &round.pull_id == pull_id)
            {
                states.push(saved);
            }
        }
        if page.len() < 128 {
            return Ok(states);
        }
        after = page.last().map(|row| row.id.clone());
    }
}

fn qualified_pull(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
    root: &RootId,
    repository: &Id,
    id: &Id,
) -> Result<(StoredLink, u64), Failure> {
    let missing = || not_found("pull_request", id);
    let link = tx
        .sessions()
        .link(id)?
        .filter(|link| {
            &link.project_id == project
                && &link.root_id == root
                && link.repository_id.as_ref() == Some(repository)
                && link.kind == "pull_request"
                && link.target_kind == "pull_request"
        })
        .ok_or_else(missing)?;
    let observed = tx
        .history()
        .event(&crate::protocol::EventId(link.event_id.clone()))?
        .filter(|event| {
            event.kind == "GitPullRequestObserved"
                && event.project_id.as_ref() == Some(project)
                && event.payload["repositoryId"].as_str() == Some(repository.as_str())
                && event.payload["pullRequestUrl"].as_str() == Some(&link.target_id)
        })
        .ok_or_else(missing)?;
    if observed.operation_id.is_none() {
        return Err(missing().into());
    }
    let number = pull_number(&link.target_id).ok_or_else(missing)?;
    Ok((link, number))
}

pub(super) fn pull_number(raw: &str) -> Option<u64> {
    let url = url::Url::parse(raw).ok().filter(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("github.com")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })?;
    let bits = url.path_segments()?.collect::<Vec<_>>();
    match bits.as_slice() {
        [_, _, "pull", number] => number.parse::<u64>().ok().filter(|parsed| {
            *parsed > 0 && *parsed <= i64::MAX as u64 && parsed.to_string() == *number
        }),
        _ => None,
    }
}

impl ReviewService {
    pub(super) fn prepare_final(
        &self,
        ctx: &InvocationContext,
        barrier: &DeliveryBarrier,
        env: &Preparation<'_>,
    ) -> Result<LocalInput, Failure> {
        let diffs = env.read(|snapshot| {
            let work =
                crate::services::work::visible_work(snapshot.work(), ctx, &barrier.root_id.0)
                    .map_err(Failure::into_storage)?;
            let merged = crate::services::git::already_merged_review_repositories(
                snapshot.history(),
                &work.project_id,
                barrier,
            )?;
            barrier
                .repositories
                .iter()
                .filter(|entry| !merged.contains(&entry.repository_id))
                .map(|entry| {
                    let link = snapshot
                        .sessions()
                        .link(&entry.pull_request_id)?
                        .filter(|link| {
                            link.project_id == work.project_id
                                && link.root_id == barrier.root_id
                                && link.repository_id.as_ref() == Some(&entry.repository_id)
                                && link.kind == "pull_request"
                        })
                        .ok_or(StorageError::ScopeDenied)?;
                    let number = pull_number(&link.target_id).ok_or(StorageError::ScopeDenied)?;
                    if !super::readiness::observed_current_pull(
                        snapshot,
                        &work.project_id,
                        &entry.repository_id,
                        number,
                        &entry.expected_head.hex,
                        &entry.observed_base.hex,
                    )? {
                        return Err(StorageError::ScopeDenied);
                    }
                    let worktree_id = serde_json::from_value::<Id>(link.body["worktreeId"].clone())
                        .map_err(|_| StorageError::ScopeDenied)?;
                    let tree = snapshot
                        .git()
                        .git_worktree(&worktree_id)?
                        .ok_or(StorageError::ScopeDenied)?;
                    Ok((
                        tree.work_id,
                        crate::protocol::records::LocalReviewDiff {
                            repository_id: entry.repository_id.clone(),
                            worktree_id,
                            base: entry.observed_base.clone(),
                            head: entry.expected_head.clone(),
                        },
                    ))
                })
                .collect::<Result<Vec<_>, StorageError>>()
        })?;
        self.capture_diffs(ctx, env, diffs)
    }

    pub(super) fn request_final(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        input: &RequestInput,
        barrier: &DeliveryBarrier,
    ) -> Result<DomainDecision, Failure> {
        let work = crate::services::work::visible_work(tx.work(), ctx, &input.root_id.0)?;
        if barrier.root_id != input.root_id
            || ctx.root_id().is_some_and(|root| root != &input.root_id)
            || (ctx.principal_kind() == PrincipalKind::Agent
                && ctx.root_id() != Some(&input.root_id))
        {
            return Err(StorageError::ScopeDenied.into());
        }
        crate::services::work::open_project(tx, ctx, &work.project_id)?;
        if input.expected_revision.get() != work.revision {
            return Err(StorageError::conflict(
                crate::protocol::RevisionDimension::Work,
                "work",
                work.id.clone(),
                input.expected_revision.clone(),
                rev(work.revision),
            )
            .into());
        }
        if &input.policy_revision != ctx.policy_revision() {
            return Err(refused(
                DomainErrorCode::SourceChanged,
                "review_policy",
                &work.id,
            ));
        }
        let stage = match (&barrier.kind, work.kind.as_str()) {
            (DeliveryKind::Feature, FEATURE) => {
                let plan = tx
                    .work()
                    .feature_plan(&work.id)?
                    .and_then(|slot| {
                        crate::services::work::model::active_of(slot.approved.as_ref())
                    })
                    .ok_or_else(|| {
                        refused(DomainErrorCode::ApprovalRequired, "feature_plan", &work.id)
                    })?;
                if plan.digest != barrier.approved_scope_digest.0 {
                    return Err(refused(
                        DomainErrorCode::SourceChanged,
                        "feature_plan",
                        &work.id,
                    ));
                }
                ReviewStage::FinalFeature
            }
            (DeliveryKind::StandaloneIssue, _)
                if work.parent_feature_id.is_none() && work.kind != FEATURE =>
            {
                ReviewStage::StandaloneIssue
            }
            _ => {
                return Err(refused(
                    DomainErrorCode::InvalidArgument,
                    "delivery_kind",
                    &work.id,
                ));
            }
        };
        let repositories = tx.work().work_repositories(&work.id)?;
        let project_repositories = tx.projects().members(&work.project_id)?;
        let requested = barrier
            .repositories
            .iter()
            .map(|entry| &entry.repository_id)
            .collect::<HashSet<_>>();
        if requested != repositories.iter().collect::<HashSet<_>>()
            || !repositories
                .iter()
                .all(|repo| project_repositories.contains(repo))
        {
            return Err(refused(
                DomainErrorCode::SourceChanged,
                "delivery_scope",
                &work.id,
            ));
        }

        let mut previous = Vec::new();
        let mut after = None;
        loop {
            let page = tx.git().code_reviews(
                Some(&work.project_id),
                Some(&input.root_id),
                after.as_ref(),
                128,
            )?;
            for row in &page {
                let state: ReviewStateRecord = decode(&row.body)?;
                if matches!(state.target, ReviewTarget::PullRequests { .. }) {
                    previous.push(state);
                }
            }
            if page.len() < 128 {
                break;
            }
            after = page.last().map(|row| row.id.clone());
        }

        let now = tx.now_ms();
        let digest = Digest(
            crate::fingerprint::canonical_digest(&json!(barrier))
                .map_err(|_| StorageError::InvalidRecord("invalid review barrier digest".into()))?,
        );
        let mut rounds = Vec::new();
        let mut sources = Vec::new();
        let mut reuse = None;
        let mut capped = None;
        let merged = crate::services::git::already_merged_review_repositories(
            tx.history(),
            &work.project_id,
            barrier,
        )?;
        for entry in &barrier.repositories {
            if merged.contains(&entry.repository_id) {
                continue;
            }
            let (_, number) = qualified_pull(
                tx,
                &work.project_id,
                &input.root_id,
                &entry.repository_id,
                &entry.pull_request_id,
            )?;
            if entry.policy_revision != input.policy_revision {
                return Err(refused(
                    DomainErrorCode::SourceChanged,
                    "review_policy",
                    &entry.repository_id,
                ));
            }
            crate::services::work::check_artifact(tx, ctx, &work.project_id, &entry.evidence)?;
            let settings = crate::settings::service::review_settings(
                tx.configuration(),
                &work.project_id,
                &entry.repository_id,
            )?;
            if !settings.final_review_required {
                continue;
            }
            if settings.github_bot_id.is_some()
                != matches!(entry.review_source, ReviewSource::GitHubAutomated)
            {
                return Err(refused(
                    DomainErrorCode::SourceChanged,
                    "review_source",
                    &entry.repository_id,
                ));
            }
            let current = previous
                .iter()
                .flat_map(|state| state.final_rounds.iter().map(move |round| (state, round)))
                .filter(|(_, round)| {
                    round.repository_id == entry.repository_id && round.pull_number == number
                })
                .max_by_key(|(_, round)| round.cycle);
            let head_digest = Digest(
                crate::fingerprint::canonical_digest(&json!({
                    "repo": entry.repository_id, "pull": number, "head": entry.expected_head,
                    "base": entry.observed_base, "policy": settings.digest,
                    "scope": barrier.approved_scope_digest,
                }))
                .map_err(|_| StorageError::InvalidRecord("invalid final review source".into()))?,
            );
            let admitted = current.map_or(0, |(_, round)| round.cycle);
            let same = current.is_some_and(|(_, round)| round.head_digest.0 == head_digest.0);
            let finished = current.is_none_or(|(state, _)| {
                !matches!(
                    state.value.state,
                    ReviewState::Pending | ReviewState::Running
                )
            });
            let cycle = match admit_round(stage, Some(settings.cap), admitted, same, finished) {
                RoundAdmission::Start(cycle) => cycle,
                RoundAdmission::Existing => {
                    reuse = current.map(|(state, _)| state.value.clone());
                    continue;
                }
                RoundAdmission::CapReached => {
                    capped = Some(entry.pull_request_id.clone());
                    continue;
                }
                RoundAdmission::WaitForOutcome => continue,
            };
            let source_id = Id::new(match settings.github_bot_id {
                Some(bot) => format!("github-bot-{bot}"),
                None => "independent-reviewer".into(),
            })
            .expect("qualified source ID");
            let source_kind = if settings.github_bot_id.is_some() {
                ReviewSourceKind::GitHubAutomation
            } else {
                ReviewSourceKind::InAppIndependent
            };
            sources.push(ReviewSourceEvidence {
                source_id,
                source_kind,
                repository_id: entry.repository_id.clone(),
                pull_request_number: PositiveCounter(number.to_string()),
                head: entry.expected_head.clone(),
                head_digest: digest.clone(),
                policy_revision: input.policy_revision.clone(),
                cycle: counter(i64::from(cycle)),
                run_id: None,
                state: ReviewObservationState::Requested,
                completed_at: None,
                finding_count: counter(0),
                zero_findings_confirmed: false,
                findings_coverage: Coverage {
                    state: CoverageState::Unavailable,
                    gaps: Vec::new(),
                    more_gaps: false,
                },
                evidence: BoundedList(Vec::new()),
            });
            // A bot review identifies the head but carries no base commit. Require a
            // new submission when an earlier review of this head used a different base.
            let reused_head = settings.github_bot_id.is_some()
                && previous.iter().any(|state| {
                    state.final_rounds.iter().any(|prior| {
                        prior.repository_id == entry.repository_id && prior.pull_number == number
                    }) && matches!(&state.target, ReviewTarget::PullRequests { barrier }
                    if barrier.repositories.iter().any(|prior| {
                        prior.repository_id == entry.repository_id
                            && prior.expected_head == entry.expected_head
                            && prior.observed_base != entry.observed_base
                    }))
                });
            rounds.push(FinalRound {
                repository_id: entry.repository_id.clone(),
                pull_id: entry.pull_request_id.clone(),
                pull_number: number,
                cycle,
                settings_digest: settings.digest,
                bot_id: settings.github_bot_id,
                requested_at_ms: now,
                fresh_bot_after_ms: reused_head.then_some(now),
                deadline_ms: now.saturating_add(settings.wait_seconds.saturating_mul(1000)),
                head_digest,
                fallback_requested: settings.github_bot_id.is_none(),
                observed_ids: Vec::new(),
                delivery_rules: None,
            });
        }
        if rounds.is_empty() {
            if let Some(id) = capped {
                return Err(refused(DomainErrorCode::Conflict, "review_round_cap", &id));
            }
            return reuse
                .map(|value| DomainDecision::NoChange {
                    result: OperationResult::ReviewsRequest { value },
                })
                .ok_or_else(|| {
                    refused(DomainErrorCode::Conflict, "review_round_pending", &work.id)
                });
        }
        let id = tx.new_id(IdKind::Link);
        let operation = OperationId(tx.new_id(IdKind::Operation));
        let prepared = command
            .prepared::<Result<LocalInput, Failure>>()
            .ok_or_else(|| refused(DomainErrorCode::Conflict, "review_preparation", &work.id))?
            .as_ref()
            .map_err(Clone::clone)?;
        let mut artifacts = Vec::with_capacity(rounds.len());
        use crate::services::artifacts::{host::HostArtifactPublication, version_ref};
        for round in &rounds {
            let patch = prepared
                .patches
                .iter()
                .find(|patch| patch.worktree.repository_id == round.repository_id)
                .ok_or(StorageError::ScopeDenied)?;
            if tx.git().git_worktree(&patch.worktree.id)?.as_ref() != Some(&patch.worktree)
                || tx
                    .projects()
                    .repository(&round.repository_id)?
                    .is_none_or(|repository| {
                        repository.revision != patch.repository_revision
                            || repository.lifecycle != "Active"
                    })
            {
                return Err(refused(
                    DomainErrorCode::SourceChanged,
                    "review_worktree",
                    &patch.worktree.id,
                ));
            }
            let version = self.artifacts.commit_host_bytes_in_scope(
                tx,
                &patch.staged,
                HostArtifactPublication {
                    title: "Exact final pull request diff",
                    media_type: "text/x-diff",
                    provenance: json!({"kind":"CodeReviewDiff","reviewId":id,
                        "repositoryId":round.repository_id,"pullRequestNumber":round.pull_number}),
                    scope: ArtifactScope::Project(work.project_id.clone()),
                    owner: ctx.principal().clone(),
                },
            )?;
            artifacts.push(super::ReviewDiffArtifact {
                repository_id: round.repository_id.clone(),
                reference: version_ref(&version),
            });
        }
        let saved = ReviewStateRecord {
            value: ReviewRecord {
                id,
                revision: rev(1),
                root_id: input.root_id.clone(),
                state: ReviewState::Pending,
                barrier: None,
                finding_ids: BoundedList(Vec::new()),
                head_digest: digest,
                sources: BoundedList(sources),
                local_review: None,
                candidate_repair: None,
            },
            target: ReviewTarget::PullRequests {
                barrier: barrier.clone(),
            },
            requested_by: ctx.principal().clone(),
            revision_of_policy: input.policy_revision.get(),
            source_artifacts: artifacts,
            local_binding: None,
            operation,
            restore_epoch: tx.store().restore_epoch,
            failure: None,
            final_rounds: rounds,
            verified_candidates: Vec::new(),
        };
        persist_request(tx, ctx, command, &work.project_id, saved)
    }
}
