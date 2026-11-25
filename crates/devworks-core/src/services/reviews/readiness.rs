//! Readiness is derived from current source and findings; a review record never waives another
//! delivery gate. A repair keeps the source's original attribution.
use super::{ReviewService, ReviewStateRecord, ReviewTarget, decode};
use crate::context::InvocationContext;
use crate::operations::reviews::ReadinessInput;
use crate::ports::storage::{GithubObservationFilter, GithubScope, ReadSnapshot, StorageError};
use crate::protocol::Id;
use crate::protocol::provider::Digest;
use crate::protocol::records::{
    CodeReviewResolution, LocalReviewTarget, ReviewFinding, ReviewObservationState,
    ReviewReadiness, ReviewState,
};
use crate::protocol::support::BoundedList;
use crate::services::github::code_review::{
    CodeReviewObservation, CodeReviewQuery, code_review_observation,
};
use crate::services::kernel::{Failure, rev};
use serde_json::json;

impl ReviewService {
    pub(super) fn readiness(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ReadinessInput,
    ) -> Result<(ReviewReadiness, i64), Failure> {
        Self::readiness_with_merged(snapshot, ctx, input, &[])
    }

    pub(crate) fn readiness_with_merged(
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ReadinessInput,
        merged: &[Id],
    ) -> Result<(ReviewReadiness, i64), Failure> {
        let root = crate::services::work::visible_work(snapshot.work(), ctx, &input.root_id.0)?;
        if ctx.root_id().is_some_and(|id| id != &input.root_id)
            || ctx.policy_revision() != &input.policy_revision
        {
            return Err(StorageError::ScopeDenied.into());
        }
        let mut out = ReviewReadiness {
            ready: false,
            review_ids: BoundedList(Vec::new()),
            blocker_ids: BoundedList(Vec::new()),
            head_digest: Digest(
                crate::fingerprint::canonical_digest(
                    &json!({"barrier":input.barrier,"localTarget":input.local_target}),
                )
                .map_err(|_| StorageError::InvalidRecord("review readiness source".into()))?,
            ),
            policy_revision: input.policy_revision.clone(),
            candidate_repairs: None,
        };
        let mut states = Vec::new();
        let mut after = None;
        loop {
            let page = snapshot.git().code_reviews(
                Some(&root.project_id),
                Some(&input.root_id),
                after.as_ref(),
                128,
            )?;
            for row in &page {
                let state: ReviewStateRecord = decode(&row.body)?;
                if state.revision_of_policy == input.policy_revision.get() {
                    states.push(state);
                }
            }
            if page.len() < 128 {
                break;
            }
            after = page.last().map(|row| row.id.clone());
        }
        if let Some(local) = &input.local_target {
            let task = crate::services::work::visible_work(snapshot.work(), ctx, &local.task_id)?;
            let selected = snapshot.work().work_repositories(&task.id)?;
            if task.parent_feature_id.as_ref() != Some(&root.id)
                || task.project_id != root.project_id
                || selected.len() != local.diffs.0.len()
                || selected
                    .iter()
                    .any(|id| !local.diffs.0.iter().any(|diff| &diff.repository_id == id))
            {
                return Err(StorageError::ScopeDenied.into());
            }
            let saved = states.iter().find(|state| match &state.target {
                ReviewTarget::Local { target } => {
                    target.task_id == local.task_id
                        && target.diffs.0.len() == local.diffs.0.len()
                        && target.diffs.0.iter().all(|left| {
                            local.diffs.0.iter().any(|right| {
                                left.repository_id == right.repository_id
                                    && left.worktree_id == right.worktree_id
                                    && left.base == right.base
                            })
                        })
                }
                _ => false,
            });
            if let Some(saved) = saved {
                out.review_ids.0.push(saved.value.id.clone());
                if let ReviewTarget::Local { target } = &saved.target {
                    if target.diffs.0.iter().all(|diff| {
                        local.diffs.0.iter().any(|candidate| {
                            candidate.repository_id == diff.repository_id
                                && candidate.head == diff.head
                        })
                    }) {
                        out.head_digest = saved.value.head_digest.clone();
                    }
                    collect_local_blockers(snapshot, saved, target, local, &mut out.blocker_ids.0)?;
                }
                if !matches!(saved.value.state, ReviewState::Ready) && out.blocker_ids.0.is_empty()
                {
                    out.blocker_ids.0.push(saved.value.id.clone());
                }
            } else {
                out.blocker_ids.0.push(local.task_id.clone());
            }
        } else if let Some(barrier) = &input.barrier {
            if barrier.root_id != input.root_id {
                return Err(StorageError::ScopeDenied.into());
            }
            let plan = snapshot
                .work()
                .feature_plan(&root.id)?
                .and_then(|slot| crate::services::work::model::active_of(slot.approved.as_ref()));
            if matches!(barrier.kind, crate::protocol::data::DeliveryKind::Feature)
                && plan.is_none_or(|plan| plan.digest != barrier.approved_scope_digest.0)
            {
                return Err(StorageError::ScopeDenied.into());
            }
            let selected = snapshot.work().work_repositories(&root.id)?;
            if selected.len() != barrier.repositories.len()
                || selected.iter().any(|id| {
                    !barrier
                        .repositories
                        .iter()
                        .any(|entry| &entry.repository_id == id)
                })
            {
                return Err(StorageError::ScopeDenied.into());
            }
            let mut repairs = Vec::new();
            for entry in &barrier.repositories {
                if merged.contains(&entry.repository_id) {
                    continue;
                }
                let Some(pull_link) =
                    snapshot
                        .sessions()
                        .link(&entry.pull_request_id)?
                        .filter(|link| {
                            link.root_id == input.root_id
                                && link.project_id == root.project_id
                                && link.target_kind == "pull_request"
                                && link.repository_id.as_ref() == Some(&entry.repository_id)
                        })
                else {
                    out.blocker_ids.0.push(entry.repository_id.clone());
                    continue;
                };
                let settings = crate::settings::service::review_settings(
                    snapshot.configuration(),
                    &root.project_id,
                    &entry.repository_id,
                )?;
                let linked_number = super::final_review::pull_number(&pull_link.target_id);
                if !settings.final_review_required {
                    if entry.policy_revision != input.policy_revision
                        || !linked_number
                            .map(|number| {
                                observed_current_pull(
                                    snapshot,
                                    &root.project_id,
                                    &entry.repository_id,
                                    number,
                                    &entry.expected_head.hex,
                                    &entry.observed_base.hex,
                                )
                            })
                            .transpose()?
                            .unwrap_or(false)
                    {
                        out.blocker_ids.0.push(entry.repository_id.clone());
                    }
                    continue;
                }
                let current = states
                    .iter()
                    .flat_map(|state| state.final_rounds.iter().map(move |round| (state, round)))
                    .filter(|(_, round)| {
                        round.repository_id == entry.repository_id
                            && Some(round.pull_number) == linked_number
                            && round.pull_id == pull_link.id
                    })
                    .max_by_key(|(_, round)| round.cycle);
                let Some((saved, round)) = current else {
                    out.blocker_ids.0.push(entry.repository_id.clone());
                    continue;
                };
                let original_target = match &saved.target {
                    ReviewTarget::PullRequests { barrier } => {
                        barrier.repositories.iter().find(|target| {
                            target.repository_id == entry.repository_id
                                && target.pull_request_id == entry.pull_request_id
                        })
                    }
                    _ => None,
                };
                let qualified = round.settings_digest == settings.digest
                    && original_target
                        .is_some_and(|original| original.observed_base == entry.observed_base)
                    && entry.policy_revision == input.policy_revision
                    && observed_current_pull(
                        snapshot,
                        &root.project_id,
                        &entry.repository_id,
                        round.pull_number,
                        &entry.expected_head.hex,
                        &entry.observed_base.hex,
                    )?;
                if !qualified {
                    out.blocker_ids.0.push(entry.repository_id.clone());
                    continue;
                }
                if saved.value.sources.0.iter().any(|source| {
                    source.repository_id == entry.repository_id
                        && matches!(
                            source.source_kind,
                            crate::protocol::records::ReviewSourceKind::InAppFallback
                        )
                        && matches!(source.state, ReviewObservationState::Running)
                }) {
                    out.blocker_ids.0.push(saved.value.id.clone());
                    continue;
                }
                let source = saved.value.sources.0.iter().find(|source| {
                    source.repository_id == entry.repository_id
                        && matches!(source.state, ReviewObservationState::Completed)
                });
                let Some(source) = source else {
                    out.blocker_ids.0.push(saved.value.id.clone());
                    continue;
                };
                if source.head != entry.expected_head {
                    let Some(repair) = saved.verified_candidates.iter().find(|repair| {
                        repair.repository_id == entry.repository_id
                            && repair.reviewed_head == source.head
                            && repair.candidate_head == entry.expected_head
                    }) else {
                        out.blocker_ids.0.push(saved.value.id.clone());
                        continue;
                    };
                    repairs.push(repair.clone());
                }
                if let Some(bot_id) = round.bot_id {
                    let number = round.pull_number.to_string();
                    let query = CodeReviewQuery {
                        project: &root.project_id,
                        repository: &entry.repository_id,
                        head: &source.head.hex,
                        pull_number: &number,
                        bot_id,
                    };
                    if let CodeReviewObservation::Completed { review, comments } =
                        code_review_observation(snapshot, &query)?
                        && (round.observed_ids.first() != Some(&review.id)
                            || comments
                                .iter()
                                .any(|comment| !round.observed_ids.contains(&comment.id)))
                    {
                        out.blocker_ids.0.push(saved.value.id.clone());
                        continue;
                    }
                }
                out.review_ids.0.push(saved.value.id.clone());
                for earlier in states.iter().filter(|state| {
                    state.final_rounds.iter().any(|prior| {
                        prior.repository_id == entry.repository_id
                            && prior.pull_number == round.pull_number
                            && prior.cycle <= round.cycle
                    })
                }) {
                    collect_open(
                        snapshot,
                        earlier,
                        &entry.repository_id,
                        &entry.expected_head,
                        &mut out.blocker_ids.0,
                    )?;
                }
            }
            repairs.truncate(128);
            out.candidate_repairs = (!repairs.is_empty()).then_some(BoundedList(repairs));
        }
        out.review_ids
            .0
            .sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        out.review_ids.0.dedup();
        out.blocker_ids
            .0
            .sort_unstable_by(|left, right| left.as_str().cmp(right.as_str()));
        out.blocker_ids.0.dedup();
        out.review_ids.0.truncate(128);
        out.blocker_ids.0.truncate(128);
        out.ready = out.blocker_ids.0.is_empty()
            && (input.barrier.is_some() || !out.review_ids.0.is_empty());
        Ok((out, rev(root.revision).get()))
    }
}

pub(super) fn collect_local_blockers(
    snapshot: &dyn ReadSnapshot,
    review: &ReviewStateRecord,
    reviewed: &LocalReviewTarget,
    requested: &LocalReviewTarget,
    blockers: &mut Vec<Id>,
) -> Result<(), Failure> {
    let mut verified = std::collections::HashSet::new();
    for id in &review.value.finding_ids.0 {
        let row = snapshot
            .git()
            .code_finding(id)?
            .ok_or(StorageError::ScopeDenied)?;
        let finding: ReviewFinding = decode(&row.body)?;
        if let Some(CodeReviewResolution::FixVerified { candidate_head, .. }) = &finding.resolution
        {
            if let Some(repo) = row.repository_id.as_ref()
                && requested
                    .diffs
                    .0
                    .iter()
                    .any(|diff| &diff.repository_id == repo && &diff.head == candidate_head)
            {
                verified.insert(repo.clone());
            } else {
                blockers.push(id.clone());
            }
        } else if super::findings::is_open(&finding) {
            blockers.push(id.clone());
        }
    }
    for original in &reviewed.diffs.0 {
        let candidate = requested
            .diffs
            .0
            .iter()
            .find(|diff| diff.repository_id == original.repository_id)
            .ok_or(StorageError::ScopeDenied)?;
        if candidate.head != original.head && !verified.contains(&candidate.repository_id) {
            blockers.push(candidate.repository_id.clone());
        }
    }
    Ok(())
}

fn collect_open(
    snapshot: &dyn ReadSnapshot,
    state: &ReviewStateRecord,
    repository: &Id,
    head: &crate::protocol::data::GitOid,
    blockers: &mut Vec<Id>,
) -> Result<(), Failure> {
    for id in &state.value.finding_ids.0 {
        let row = snapshot
            .git()
            .code_finding(id)?
            .ok_or(StorageError::ScopeDenied)?;
        if row
            .repository_id
            .as_ref()
            .is_some_and(|id| id != repository)
        {
            continue;
        }
        let finding: ReviewFinding = decode(&row.body)?;
        if super::findings::is_open(&finding)
            || matches!(&finding.resolution, Some(CodeReviewResolution::FixVerified { candidate_head, .. })
                if candidate_head != head)
        {
            blockers.push(id.clone());
        }
    }
    Ok(())
}

pub(super) fn observed_current_pull(
    snapshot: &dyn ReadSnapshot,
    project: &crate::protocol::ProjectId,
    repository: &Id,
    number: u64,
    head: &str,
    base: &str,
) -> Result<bool, StorageError> {
    if !snapshot.projects().members(project)?.contains(repository)
        || snapshot
            .github()
            .github_repository_binding(repository)?
            .is_none_or(|binding| binding.state != "Active")
    {
        return Ok(false);
    }
    let filter = GithubObservationFilter {
        scope: GithubScope {
            repositories: vec![repository.clone()],
            project_id: Some(project.clone()),
        },
        kinds: vec!["PullRequest".into()],
        subject_id: Some(number.to_string()),
        head: None,
    };
    let mut after = None;
    let mut latest = None;
    for _ in 0..20 {
        let page = snapshot
            .github()
            .github_observations(&filter, after.as_ref(), 200)?;
        for row in &page {
            if latest.as_ref().is_none_or(|(stamp, _, _, _, _)| {
                *stamp <= (row.source_updated_at_ms, row.observed_at_ms)
            }) {
                latest = Some((
                    (row.source_updated_at_ms, row.observed_at_ms),
                    row.head.clone(),
                    row.payload["state"].as_str().map(str::to_owned),
                    row.payload["baseRef"].as_str().map(str::to_owned),
                    row.payload["baseHead"].as_str().map(str::to_owned),
                ));
            }
        }
        if page.len() < 200 {
            return Ok(
                latest.is_some_and(|(_, actual_head, state, target, actual_base)| {
                    actual_head.as_deref() == Some(head)
                        && state.as_deref() == Some("open")
                        && target.as_deref() == Some("main")
                        && actual_base.as_deref() == Some(base)
                }),
            );
        }
        after = page.last().map(|row| row.id.clone());
    }
    Ok(false)
}
