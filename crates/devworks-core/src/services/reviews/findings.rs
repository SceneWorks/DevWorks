//! Finding replies and dispositions keep the original source and actor attribution.
use super::commands::refused;
use super::{ReviewService, ReviewStateRecord, ReviewTarget, decode};
use crate::context::{InvocationContext, PrincipalKind};
use crate::operations::reviews::{AdjudicateInput, RespondInput};
use crate::ports::storage::{
    NewEvent, StorageError, StoredCodeFinding, StoredCodeReview, WriteTransaction,
};
use crate::ports::{IdKind, Preparation};
use crate::protocol::records::{
    CodeReviewResolution, CodeReviewSeverity, ReviewFinding, ReviewState, WorkItemType,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{DomainErrorCode, EventId, Id, Revision, RevisionDimension};
use crate::services::kernel::{Failure, not_found, rev};
use serde_json::json;

fn scoped(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    id: &Id,
    expected: &Revision,
) -> Result<
    (
        StoredCodeFinding,
        ReviewFinding,
        StoredCodeReview,
        ReviewStateRecord,
    ),
    Failure,
> {
    let missing = || not_found("review_finding", id);
    let row = tx.git().code_finding(id)?.ok_or_else(missing)?;
    let root = crate::services::work::visible_work(tx.work(), ctx, &row.root_id.0)?;
    if root.project_id != row.project_id
        || ctx.root_id().is_some_and(|current| current != &row.root_id)
    {
        return Err(missing().into());
    }
    crate::services::work::open_project(tx, ctx, &row.project_id)?;
    if row.revision != expected.get() {
        return Err(StorageError::conflict(
            RevisionDimension::Review,
            "review_finding",
            id.clone(),
            expected.clone(),
            rev(row.revision),
        )
        .into());
    }
    let review = tx
        .git()
        .code_review(&row.review_id)?
        .filter(|owner| owner.project_id == row.project_id && owner.root_id == row.root_id)
        .ok_or_else(missing)?;
    let finding = decode(&row.body)?;
    let state: ReviewStateRecord = decode(&review.body)?;
    if state.revision_of_policy != ctx.policy_revision().get() {
        return Err(refused(
            DomainErrorCode::SourceChanged,
            "review_policy",
            &review.id,
        ));
    }
    Ok((row, finding, review, state))
}

fn save(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    mut row: StoredCodeFinding,
    mut finding: ReviewFinding,
    kind: &'static str,
    details: serde_json::Value,
) -> Result<ReviewFinding, Failure> {
    let prior = row.revision;
    row.revision = prior
        .checked_add(1)
        .ok_or_else(|| StorageError::InvalidRecord("code review finding revision".into()))?;
    finding.revision = rev(row.revision);
    row.body = serde_json::to_value(&finding)
        .map_err(|_| StorageError::InvalidRecord("code review finding body".into()))?;
    tx.git().put_code_finding(&row, Some(prior))?;
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event,
        actor: ctx.principal().clone(),
        project_id: Some(row.project_id.clone()),
        root_id: Some(row.root_id),
        operation_id: None,
        aggregate_kind: "code_review_finding".into(),
        aggregate_id: row.id.as_str().into(),
        aggregate_revision: row.revision,
        kind: kind.into(),
        payload: json!({"reviewId":row.review_id,"findingId":row.id,"details":details}),
    })?;
    Ok(finding)
}

impl ReviewService {
    /// Refresh a scoped PR conversation before validating a link that has no GitHub inline parent.
    pub(super) fn refresh_followup(
        &self,
        ctx: &InvocationContext,
        input: &AdjudicateInput,
        env: &Preparation<'_>,
    ) -> Result<(), StorageError> {
        let Some(github) = self.github.as_ref() else {
            return Ok(());
        };
        let Some(CodeReviewResolution::Followup {
            review_thread_reply,
            ..
        }) = &input.resolution
        else {
            return Ok(());
        };
        let inline = review_thread_reply.0.contains("#discussion_r");
        if !inline && !review_thread_reply.0.contains("#issuecomment-") {
            return Ok(());
        }
        let target = env.read(|snapshot| {
            let row = snapshot
                .git()
                .code_finding(&input.finding_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let (_, state) = super::visible_review(snapshot, ctx, &row.review_id)
                .map_err(Failure::into_storage)?;
            let Some(repository) = row.repository_id else {
                return Ok(None);
            };
            Ok(state
                .final_rounds
                .iter()
                .find(|round| round.repository_id == repository)
                .map(|round| (repository, round.pull_number)))
        })?;
        if let Some((repository, number)) = target {
            // If a cached page is already available it can still verify the exact reply.
            let _ = github.poll_review_conversation(&repository, number, inline);
        }
        Ok(())
    }

    pub(super) fn respond(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &RespondInput,
    ) -> Result<DomainDecision, Failure> {
        let (row, mut finding, _, _) =
            scoped(tx, ctx, &input.finding_id, &input.expected_revision)?;
        if input.response.0.trim().is_empty() {
            return Err(refused(
                DomainErrorCode::InvalidArgument,
                "review_response",
                &row.id,
            ));
        }
        for item in &input.evidence.0 {
            crate::services::work::check_artifact(tx, ctx, &row.project_id, item)?;
        }
        finding.response = Some(input.response.clone());
        for evidence in &input.evidence.0 {
            if !finding.evidence.0.contains(evidence) {
                finding.evidence.0.push(evidence.clone());
            }
        }
        if finding.evidence.0.len() > 128 {
            return Err(refused(
                DomainErrorCode::InvalidArgument,
                "review_evidence",
                &row.id,
            ));
        }
        let value = save(
            tx,
            ctx,
            row,
            finding,
            "CodeReviewFindingResponded",
            json!({"response":input.response,"evidence":input.evidence}),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::ReviewsRespond { value },
        })
    }

    pub(super) fn adjudicate(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &crate::gateway::AdmittedCommand,
        input: &AdjudicateInput,
    ) -> Result<DomainDecision, Failure> {
        let (row, mut finding, mut review, mut state) =
            scoped(tx, ctx, &input.finding_id, &input.expected_revision)?;
        let human = ctx.principal_kind() == PrincipalKind::Human;
        if input.rationale.0.trim().is_empty() {
            return Err(refused(
                DomainErrorCode::InvalidArgument,
                "review_rationale",
                &row.id,
            ));
        }
        if !input.actionable
            && !human
            && (ctx.principal() == &state.requested_by
                || !state
                    .local_binding
                    .as_ref()
                    .is_some_and(|binding| &binding.actor == ctx.principal()))
        {
            return Err(refused(
                DomainErrorCode::PermissionDenied,
                "review_adjudicator",
                &row.id,
            ));
        }
        for item in &input.evidence.0 {
            crate::services::work::check_artifact(tx, ctx, &row.project_id, item)?;
        }
        if let Some(disposition) = input.resolution.as_ref() {
            match disposition {
                CodeReviewResolution::NotActionable {} => {}
                CodeReviewResolution::FixVerified {
                    candidate_head,
                    focused_check_ids,
                } => {
                    if !verified_final_fix(tx, &row, &state, candidate_head, &focused_check_ids.0)?
                        && !super::local_fix::qualified(
                            tx,
                            ctx,
                            command,
                            &row,
                            &state,
                            candidate_head,
                            &focused_check_ids.0,
                        )?
                    {
                        return Err(refused(
                            DomainErrorCode::SourceChanged,
                            "review_fix_evidence",
                            &row.id,
                        ));
                    }
                }
                CodeReviewResolution::Followup {
                    bug_id,
                    review_thread_reply,
                } => {
                    let bug = crate::services::work::visible_work(tx.work(), ctx, bug_id)?;
                    if bug.project_id != row.project_id
                        || !matches!(
                            crate::kernel::work_brief::WorkBrief::of(&bug)?.task_type,
                            Some(WorkItemType::Bug)
                        )
                        || !matches!(
                            finding.code_severity,
                            Some(CodeReviewSeverity::Minor | CodeReviewSeverity::Informational)
                        )
                    {
                        return Err(refused(
                            DomainErrorCode::InvalidArgument,
                            "review_followup_bug",
                            bug_id,
                        ));
                    }
                    if !verified_thread_reply(tx, &row, &state, &review_thread_reply.0, bug_id)? {
                        return Err(refused(
                            DomainErrorCode::SourceChanged,
                            "review_thread_reply",
                            &row.id,
                        ));
                    }
                }
                CodeReviewResolution::NeedsHuman { decision_id } => {
                    if tx
                        .decisions()
                        .approval(decision_id)?
                        .is_none_or(|approval| {
                            approval.scope
                                != crate::ports::storage::ConfigScope::Project(
                                    row.project_id.clone(),
                                )
                                || approval.state != "Pending"
                        })
                    {
                        return Err(not_found("review_decision", decision_id).into());
                    }
                }
            }
        }
        finding.actionable = input.actionable;
        finding.resolution = input
            .resolution
            .clone()
            .or_else(|| (!input.actionable).then_some(CodeReviewResolution::NotActionable {}));
        let value = save(
            tx,
            ctx,
            row,
            finding,
            "CodeReviewFindingAdjudicated",
            json!({"rationale":input.rationale,"resolution":input.resolution,
                "evidence":input.evidence,"actionable":input.actionable}),
        )?;
        match state.target {
            ReviewTarget::Local { .. } => {
                if state.value.local_review.as_ref().is_some_and(|local| {
                    matches!(
                        local.state,
                        crate::protocol::records::ReviewObservationState::Completed
                    )
                }) {
                    state.value.state = if has_open_findings(tx, &state)? {
                        ReviewState::ChangesRequired
                    } else {
                        ReviewState::Ready
                    };
                    super::local::write_local(
                        tx,
                        &mut review,
                        &mut state,
                        ctx,
                        "CodeReviewReadinessChanged",
                    )?;
                }
            }
            ReviewTarget::PullRequests { .. } => {
                super::remote::update_final_state(tx, &mut review, &mut state)?
            }
        }
        Ok(DomainDecision::Applied {
            result: OperationResult::ReviewsAdjudicate { value },
        })
    }
}

fn verified_final_fix(
    tx: &mut dyn WriteTransaction,
    finding: &StoredCodeFinding,
    original: &ReviewStateRecord,
    head: &crate::protocol::data::GitOid,
    check_ids: &[Id],
) -> Result<bool, Failure> {
    use crate::ports::storage::GithubScope;
    use crate::protocol::records::{ReviewChangeReason, ReviewObservationState};

    let Some(repository) = &finding.repository_id else {
        return Ok(false);
    };
    let Some(first) = original
        .final_rounds
        .iter()
        .find(|round| &round.repository_id == repository)
    else {
        return Ok(false);
    };
    if check_ids.is_empty() {
        return Ok(false);
    }
    let lineage = super::final_review::review_lineage(
        tx,
        &finding.project_id,
        &finding.root_id,
        repository,
        &first.pull_id,
    )?;
    let last = lineage
        .iter()
        .flat_map(|state| state.final_rounds.iter().map(move |round| (state, round)))
        .filter(|(_, round)| &round.repository_id == repository && round.pull_id == first.pull_id)
        .max_by_key(|(_, round)| round.cycle);
    let Some((last_review, last_round)) = last else {
        return Ok(false);
    };
    let settings = crate::settings::service::review_settings(
        tx.configuration(),
        &finding.project_id,
        repository,
    )?;
    if settings.digest != last_round.settings_digest {
        return Ok(false);
    }
    if last_review.verified_candidates.iter().any(|proof| {
        &proof.candidate_head == head && &proof.repository_id == repository
            && proof.changes.0.iter().any(|change| matches!(
                &change.reason, ReviewChangeReason::Finding { finding_id } if finding_id == &finding.id
            ))
            && check_ids.iter().all(|id| proof.focused_check_ids.0.contains(id))
    }) {
        return Ok(true);
    }
    // A later review and separately observed passing checks can verify an ordinary in-round fix.
    if last_round.cycle <= first.cycle
        || !last_review.value.sources.0.iter().any(|source| {
            &source.repository_id == repository
                && &source.head == head
                && matches!(source.state, ReviewObservationState::Completed)
        })
    {
        return Ok(false);
    }
    let scope = GithubScope {
        repositories: vec![repository.clone()],
        project_id: Some(finding.project_id.clone()),
    };
    for id in check_ids {
        if tx
            .github()
            .github_observation(id, &scope)?
            .is_none_or(|check| {
                check.head.as_deref() != Some(head.hex.as_str())
                    || !super::candidate::passed_check(&check)
            })
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn verified_thread_reply(
    tx: &mut dyn WriteTransaction,
    row: &StoredCodeFinding,
    state: &ReviewStateRecord,
    url: &str,
    bug: &Id,
) -> Result<bool, StorageError> {
    use crate::ports::storage::{GithubObservationFilter, GithubScope};
    let ReviewTarget::PullRequests { barrier } = &state.target else {
        return Ok(false);
    };
    let Some(repository) = &row.repository_id else {
        return Ok(false);
    };
    let Some(round) = state
        .final_rounds
        .iter()
        .find(|round| &round.repository_id == repository)
    else {
        return Ok(false);
    };
    let settings =
        crate::settings::service::review_settings(tx.configuration(), &row.project_id, repository)?;
    let stage = match barrier.kind {
        crate::protocol::data::DeliveryKind::Feature => super::policy::ReviewStage::FinalFeature,
        crate::protocol::data::DeliveryKind::StandaloneIssue => {
            super::policy::ReviewStage::StandaloneIssue
        }
    };
    let lineage = super::final_review::review_lineage(
        tx,
        &row.project_id,
        &row.root_id,
        repository,
        &round.pull_id,
    )
    .map_err(Failure::into_storage)?;
    if !lineage
        .iter()
        .flat_map(|record| &record.final_rounds)
        .any(|latest| {
            &latest.repository_id == repository
                && latest.pull_id == round.pull_id
                && latest.cycle >= stage.round_limit(Some(settings.cap))
                && latest.settings_digest == settings.digest
        })
    {
        return Ok(false);
    }
    let scope = GithubScope {
        repositories: vec![repository.clone()],
        project_id: Some(row.project_id.clone()),
    };
    let original = row
        .source_observation_id
        .as_ref()
        .map(|id| tx.github().github_observation(id, &scope))
        .transpose()?
        .flatten();
    let inline_parent = original
        .as_ref()
        .filter(|item| item.kind == "ReviewComment")
        .and_then(|item| item.payload["resourceId"].as_str())
        .and_then(|id| id.parse::<u64>().ok());
    let Some(target) = barrier
        .repositories
        .iter()
        .find(|entry| &entry.repository_id == repository)
    else {
        return Ok(false);
    };
    let filter = GithubObservationFilter {
        scope,
        kinds: vec![
            if inline_parent.is_some() {
                "ReviewComment"
            } else {
                "PullConversationComment"
            }
            .into(),
        ],
        subject_id: Some(round.pull_number.to_string()),
        head: None,
    };
    let mut after = None;
    for _ in 0..20 {
        let page = tx
            .github()
            .github_observations(&filter, after.as_ref(), 200)?;
        if page.iter().any(|reply| {
            reply.url == url
                && reply.payload["authorType"] == "User"
                && reply.payload["body"].as_str().is_some_and(|text| {
                    text.contains(bug.as_str())
                        && match inline_parent {
                            Some(parent) => reply.payload["inReplyToId"].as_u64() == Some(parent),
                            None => {
                                text.contains(row.id.as_str())
                                    && text.contains(&target.expected_head.hex)
                            }
                        }
                })
        }) {
            return Ok(true);
        }
        if page.len() < 200 {
            return Ok(false);
        }
        after = page.last().map(|reply| reply.id.clone());
    }
    Ok(false)
}

pub(super) fn has_open_findings(
    tx: &mut dyn WriteTransaction,
    state: &ReviewStateRecord,
) -> Result<bool, StorageError> {
    for id in &state.value.finding_ids.0 {
        let row = tx
            .git()
            .code_finding(id)?
            .ok_or(StorageError::ScopeDenied)?;
        let finding: ReviewFinding = decode(&row.body).map_err(Failure::into_storage)?;
        if is_open(&finding) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn is_open(finding: &ReviewFinding) -> bool {
    finding.actionable
        && !matches!(
            finding.resolution,
            Some(
                CodeReviewResolution::NotActionable {}
                    | CodeReviewResolution::FixVerified { .. }
                    | CodeReviewResolution::Followup { .. }
            )
        )
}
