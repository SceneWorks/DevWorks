//! Observe an admitted GitHub automation attempt and preserve the exact source and findings.
use super::{ReviewService, ReviewStateRecord, ReviewTarget, decode};
use crate::ports::IdKind;
use crate::ports::storage::{
    NewEvent, OperationRecord, ReadOptions, StorageError, StoredCodeFinding, StoredCodeReview,
    StoredGithubObservation, WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::protocol::records::{
    CodeReviewSeverity, ReviewFinding, ReviewObservationState, ReviewSourceKind, ReviewState,
    Severity,
};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{EventId, Id};
use crate::services::github::code_review::{
    CodeReviewObservation, CodeReviewQuery, code_review_observation,
};
use crate::services::kernel::{Failure, content, counter, rev};
use serde_json::json;

impl ReviewService {
    /// One pass in the shared effect worker. Replays append neither a second finding nor
    /// another fallback authorization for the same exact final-PR round.
    pub fn poll_final_review(
        &self,
        operation: &OperationRecord,
        refresh: bool,
    ) -> Result<(), StorageError> {
        self.observe_final(operation, refresh)
    }

    fn observe_final(
        &self,
        operation: &OperationRecord,
        refresh: bool,
    ) -> Result<(), StorageError> {
        if operation.capability != "reviews.request" || operation.schema_version != 1 {
            return Err(StorageError::ScopeDenied);
        }
        let id = serde_json::from_value::<Id>(operation.payload["reviewId"].clone())
            .map_err(|_| StorageError::ScopeDenied)?;
        let (row, saved) =
            read_snapshot(self.store.as_ref(), &ReadOptions::default(), |snapshot| {
                let row = snapshot
                    .git()
                    .code_review(&id)?
                    .ok_or(StorageError::ScopeDenied)?;
                let saved: ReviewStateRecord = decode(&row.body).map_err(Failure::into_storage)?;
                if saved.operation != operation.id
                    || operation.root_id.as_ref() != Some(&row.root_id)
                {
                    return Err(StorageError::ScopeDenied);
                }
                Ok((row, saved))
            })?;
        let ReviewTarget::PullRequests { barrier } = saved.target else {
            return Ok(());
        };
        let now = self.clock.now_ms().get();
        let mut fetched = Vec::new();
        for round in &saved.final_rounds {
            let source = barrier
                .repositories
                .iter()
                .find(|repo| repo.repository_id == round.repository_id)
                .ok_or(StorageError::ScopeDenied)?;
            let rules = if refresh && round.delivery_rules.is_none() {
                self.github.as_ref().and_then(|github| {
                    github
                        .delivery_review_rules(&row.project_id, &round.repository_id)
                        .ok()
                })
            } else {
                None
            };
            let Some(bot_id) = round.bot_id else {
                if rules.is_some() {
                    fetched.push((round.repository_id.clone(), rules, None));
                }
                continue;
            };
            let pull_number = round.pull_number.to_string();
            let query = CodeReviewQuery {
                project: &row.project_id,
                repository: &round.repository_id,
                head: &source.expected_head.hex,
                pull_number: &pull_number,
                bot_id,
                requested_at_ms: round.requested_at_ms,
            };
            let observed = if !refresh {
                read_snapshot(self.store.as_ref(), &ReadOptions::default(), |snapshot| {
                    code_review_observation(snapshot, &query)
                })?
            } else if let Some(github) = &self.github {
                match github.poll_code_review(query) {
                    Ok(observed) => observed,
                    Err(Failure::Storage(StorageError::Cancelled)) => {
                        return Err(StorageError::Cancelled);
                    }
                    Err(_) => CodeReviewObservation::Waiting,
                }
            } else {
                CodeReviewObservation::Unavailable
            };
            fetched.push((round.repository_id.clone(), rules, Some(observed)));
        }
        if fetched.is_empty() {
            return Ok(());
        }
        let expected = row.revision;
        run_write(self.store.as_ref(), WriteOptions::observed(), move |tx| {
            let mut current = tx
                .git()
                .code_review(&id)?
                .ok_or(StorageError::ScopeDenied)?;
            if current.revision != expected {
                return Ok(());
            }
            let mut state: ReviewStateRecord =
                decode(&current.body).map_err(Failure::into_storage)?;
            let mut changed = false;
            for (repository, rules, observed) in fetched {
                let index = state
                    .final_rounds
                    .iter()
                    .position(|round| round.repository_id == repository)
                    .ok_or(StorageError::ScopeDenied)?;
                let round = &mut state.final_rounds[index];
                if let Some(rules) = rules {
                    round.delivery_rules = Some(rules);
                    changed = true;
                }
                let Some(observed) = observed else {
                    continue;
                };
                let Some(source) = state.value.sources.0.iter_mut().find(|source| {
                    source.repository_id == repository
                        && matches!(source.source_kind, ReviewSourceKind::GitHubAutomation)
                }) else {
                    return Err(StorageError::ScopeDenied);
                };
                let policy = crate::settings::service::review_settings(
                    tx.configuration(),
                    &current.project_id,
                    &repository,
                )?;
                if policy.digest != round.settings_digest
                    || matches!(observed, CodeReviewObservation::Stale)
                {
                    if !matches!(
                        source.state,
                        ReviewObservationState::Stale | ReviewObservationState::Completed
                    ) {
                        source.state = ReviewObservationState::Stale;
                        changed = true;
                    }
                    continue;
                }
                match observed {
                    CodeReviewObservation::Completed { review, comments } => {
                        if round.observed_ids.first() == Some(&review.id)
                            && comments
                                .iter()
                                .all(|comment| round.observed_ids.contains(&comment.id))
                        {
                            continue;
                        }
                        let mut added = Vec::new();
                        for comment in &comments {
                            if round.observed_ids.contains(&comment.id) {
                                continue;
                            }
                            if let Some(id) =
                                publish_finding(tx, &current, &repository, comment, false)?
                            {
                                added.push(id);
                            }
                        }
                        let needs_summary = review.payload["state"] == "CHANGES_REQUESTED"
                            && (comments.is_empty()
                                || review.payload["body"]
                                    .as_str()
                                    .is_some_and(|text| !text.trim().is_empty()));
                        if needs_summary
                            && !round.observed_ids.contains(&review.id)
                            && let Some(id) =
                                publish_finding(tx, &current, &repository, &review, true)?
                        {
                            added.push(id);
                        }
                        round.observed_ids.clear();
                        round.observed_ids.push(review.id.clone());
                        round
                            .observed_ids
                            .extend(comments.iter().map(|comment| comment.id.clone()));
                        state.value.finding_ids.0.extend(added);
                        source.run_id = Some(review.id);
                        source.state = ReviewObservationState::Completed;
                        source.completed_at = Some(counter(now));
                        let count = comments.len() + usize::from(needs_summary);
                        source.finding_count = counter(count as i64);
                        source.zero_findings_confirmed = count == 0;
                        source.findings_coverage.state =
                            crate::protocol::data::CoverageState::Complete;
                        changed = true;
                    }
                    CodeReviewObservation::Failed
                    | CodeReviewObservation::Unavailable
                    | CodeReviewObservation::Waiting
                        if !round.fallback_requested
                            && !matches!(source.state, ReviewObservationState::Completed)
                            && (now >= round.deadline_ms
                                || !matches!(observed, CodeReviewObservation::Waiting)) =>
                    {
                        round.fallback_requested = true;
                        source.state = match observed {
                            CodeReviewObservation::Failed => ReviewObservationState::Failed,
                            CodeReviewObservation::Unavailable => {
                                ReviewObservationState::Unavailable
                            }
                            _ => ReviewObservationState::TimedOut,
                        };
                        changed = true;
                    }
                    _ => {}
                }
            }
            if changed {
                update_final_state(tx, &mut current, &mut state)?;
            }
            Ok(())
        })
    }
    /// Reconcile late observations after an independent fallback completes. The GitHub worker
    /// already watches open PRs, so this pass reads its saved results without extra HTTP calls.
    pub fn reconcile_late_page(&self, after: Option<&Id>) -> Result<Option<Id>, StorageError> {
        let (operations, next) =
            read_snapshot(self.store.as_ref(), &ReadOptions::default(), |snapshot| {
                let page = snapshot.git().code_reviews(None, None, after, 32)?;
                let next =
                    (page.len() == 32).then(|| page.last().expect("nonempty page").id.clone());
                let mut operations = Vec::new();
                for row in page {
                    if !matches!(row.state.as_str(), "Ready" | "ChangesRequired") {
                        continue;
                    }
                    let saved: ReviewStateRecord =
                        decode(&row.body).map_err(Failure::into_storage)?;
                    if saved
                        .final_rounds
                        .iter()
                        .any(|round| round.bot_id.is_some())
                        && snapshot
                            .work()
                            .work(&row.root_id.0)?
                            .is_some_and(|root| root.completion.is_none())
                        && let Some(operation) =
                            snapshot.operations().operation(&saved.operation)?
                    {
                        operations.push(operation);
                    }
                }
                Ok((operations, next))
            })?;
        for operation in operations {
            self.observe_final(&operation, false)?;
        }
        Ok(next)
    }
}

fn publish_finding(
    tx: &mut dyn WriteTransaction,
    review: &StoredCodeReview,
    repository: &Id,
    observation: &StoredGithubObservation,
    overall: bool,
) -> Result<Option<Id>, StorageError> {
    let digest = crate::fingerprint::sha256_hex(
        format!("{}:{}", review.id.as_str(), observation.id.as_str()).as_bytes(),
    );
    let id = Id::new(format!("review-finding-{}", &digest[..48])).expect("opaque finding ID");
    if tx.git().code_finding(&id)?.is_some() {
        return Ok(None);
    }
    let body: String = observation.payload["body"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(4096)
        .collect();
    let summary = if body.trim().is_empty() {
        if overall {
            "The configured reviewer requested changes."
        } else {
            "Review comment"
        }
    } else {
        &body
    };
    let location = match (
        observation.payload["path"].as_str(),
        observation.payload["line"].as_u64(),
    ) {
        (Some(path), Some(line)) => format!("{path}:{line} · {}", observation.url),
        _ => observation.url.clone(),
    };
    let value = ReviewFinding {
        id: id.clone(),
        revision: rev(1),
        review_id: review.id.clone(),
        summary: content(summary),
        location: Some(BoundedText(location.chars().take(1024).collect())),
        severity: if overall {
            Severity::Error
        } else {
            Severity::Warning
        },
        actionable: true,
        response: None,
        evidence: BoundedList(Vec::new()),
        code_severity: Some(if overall {
            CodeReviewSeverity::Major
        } else {
            CodeReviewSeverity::Minor
        }),
        resolution: None,
    };
    tx.git().put_code_finding(
        &StoredCodeFinding {
            id: id.clone(),
            review_id: review.id.clone(),
            repository_id: Some(repository.clone()),
            source_observation_id: Some(observation.id.clone()),
            project_id: review.project_id.clone(),
            root_id: review.root_id.clone(),
            revision: 1,
            body: serde_json::to_value(value)
                .map_err(|_| StorageError::InvalidRecord("invalid remote finding".into()))?,
        },
        None,
    )?;
    Ok(Some(id))
}

pub(super) fn update_final_state(
    tx: &mut dyn WriteTransaction,
    row: &mut StoredCodeReview,
    saved: &mut ReviewStateRecord,
) -> Result<(), StorageError> {
    let stale = saved
        .value
        .sources
        .0
        .iter()
        .any(|source| matches!(source.state, ReviewObservationState::Stale));
    let bound_reviewer_running = saved.value.sources.0.iter().any(|source| {
        matches!(
            source.source_kind,
            ReviewSourceKind::InAppFallback | ReviewSourceKind::InAppIndependent
        ) && matches!(source.state, ReviewObservationState::Running)
    });
    let complete = !bound_reviewer_running
        && saved.final_rounds.iter().all(|round| {
            saved.value.sources.0.iter().any(|source| {
                source.repository_id == round.repository_id
                    && matches!(source.state, ReviewObservationState::Completed)
            })
        });
    saved.value.state = if stale {
        ReviewState::Blocked
    } else if complete && !super::findings::has_open_findings(tx, saved)? {
        ReviewState::Ready
    } else if complete {
        ReviewState::ChangesRequired
    } else {
        ReviewState::Running
    };
    let prior = row.revision;
    row.revision = prior
        .checked_add(1)
        .ok_or_else(|| StorageError::InvalidRecord("review revision overflow".into()))?;
    saved.value.revision = rev(row.revision);
    row.body = serde_json::to_value(&saved)
        .map_err(|_| StorageError::InvalidRecord("invalid remote review body".into()))?;
    row.state = row.body["value"]["state"]
        .as_str()
        .ok_or(StorageError::ScopeDenied)?
        .to_owned();
    tx.git().put_code_review(row, Some(prior))?;
    let event = EventId(tx.new_id(IdKind::Event));
    let actor = tx.store().host_actor.clone();
    tx.history().append(NewEvent {
        id: event,
        actor,
        project_id: Some(row.project_id.clone()),
        root_id: Some(row.root_id.clone()),
        operation_id: Some(saved.operation.clone()),
        aggregate_kind: "code_review".into(),
        aggregate_id: row.id.as_str().into(),
        aggregate_revision: row.revision,
        kind: "CodeReviewSourceObserved".into(),
        payload: json!({"reviewId":row.id,"state":row.state, "sources":saved.value.sources}),
    })?;
    Ok(())
}
