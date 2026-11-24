//! The independent reviewer is a native child assignment with an immutable diff input.
use super::{ReviewService, ReviewStateRecord, ReviewTarget, decode};
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::IdKind;
use crate::ports::storage::{
    ArtifactScope, GitRead, NewEvent, ReadOptions, ReadSnapshot, StorageError, StoredCodeFinding,
    StoredCodeReview, WriteOptions, WriteTransaction, read_snapshot, run_write,
};
use crate::protocol::records::{
    AgentRecord, CodeReviewSeverity, ReviewFinding, ReviewObservationState, ReviewState, Severity,
};
use crate::protocol::support::{BoundedList, BoundedText};
use crate::protocol::{ActorId, AssignmentId, EventId, Id, RootId};
use crate::services::agents::AgentService;
use crate::services::artifacts::{host::HostArtifactPublication, version_ref};
use crate::services::kernel::{Failure, content, counter, rev};
use crate::settings::catalog::AgentRole;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LocalBinding {
    assignment_id: AssignmentId,
    session_id: Id,
    pub(super) actor: ActorId,
    repositories: Vec<Id>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodeReviewReport {
    pub findings: Vec<CodeReviewReportFinding>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodeReviewReportFinding {
    pub summary: String,
    pub location: String,
    pub severity: CodeReviewSeverity,
    pub repository_id: Option<Id>,
}

#[derive(Clone)]
pub struct LocalReviewInput {
    pub review_id: Id,
    pub diff_digest: String,
    pub source_artifacts: Vec<super::ReviewDiffArtifact>,
}

fn pending_for_root(
    git: &dyn GitRead,
    root: &RootId,
    binding: Option<(&AssignmentId, &Id, &ActorId)>,
    mut after: Option<Id>,
) -> Result<Option<(StoredCodeReview, ReviewStateRecord)>, Failure> {
    loop {
        let rows = git.code_reviews(None, Some(root), after.as_ref(), 128)?;
        for row in &rows {
            let state: ReviewStateRecord = decode(&row.body)?;
            let matches = match (binding, state.local_binding.as_ref()) {
                (None, None) => match state.target {
                    ReviewTarget::Local { .. } => matches!(state.value.state, ReviewState::Pending),
                    ReviewTarget::PullRequests { .. } => ready_for_fallback(&state),
                },
                (Some((assignment, session, actor)), Some(current)) => {
                    &current.assignment_id == assignment
                        && &current.session_id == session
                        && &current.actor == actor
                        && matches!(state.value.state, ReviewState::Running)
                }
                _ => false,
            };
            if matches {
                return Ok(Some((row.clone(), state)));
            }
        }
        if rows.len() < 128 {
            return Ok(None);
        }
        after = rows.last().map(|row| row.id.clone());
    }
}

fn ready_for_fallback(state: &ReviewStateRecord) -> bool {
    !matches!(
        state.value.state,
        ReviewState::Blocked | ReviewState::Ready | ReviewState::ChangesRequired
    ) && state
        .final_rounds
        .iter()
        .any(|round| round.fallback_requested)
        && state.final_rounds.iter().all(|round| {
            round.fallback_requested
                || state.value.sources.0.iter().any(|source| {
                    source.repository_id == round.repository_id
                        && matches!(source.state, ReviewObservationState::Completed)
                })
        })
}

impl ReviewService {
    pub fn pending_for_coordinator(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
    ) -> Result<bool, StorageError> {
        let (Some(root), Some(project)) = (ctx.root_id(), ctx.project_id()) else {
            return Ok(false);
        };
        let mut after = None;
        loop {
            let rows =
                snapshot
                    .git()
                    .code_reviews(Some(project), Some(root), after.as_ref(), 128)?;
            for row in &rows {
                if !matches!(row.state.as_str(), "Pending" | "Running") {
                    continue;
                }
                let state: ReviewStateRecord = decode(&row.body).map_err(Failure::into_storage)?;
                if state.restore_epoch == ctx.restore_epoch().get()
                    && state.revision_of_policy == ctx.policy_revision().get()
                    && snapshot
                        .operations()
                        .operation(&state.operation)?
                        .is_some_and(|operation| {
                            !crate::ports::storage::is_terminal(operation.state)
                        })
                {
                    return Ok(true);
                }
            }
            if rows.len() < 128 {
                return Ok(false);
            }
            after = rows.last().map(|row| row.id.clone());
        }
    }

    /// Called by the native coordinator before it advances past the queued review.
    /// Child creation and the unique binding of that exact review commit atomically.
    pub fn schedule_local(
        &self,
        tx: &mut dyn WriteTransaction,
        agents: &AgentService,
        coordinator: &InvocationContext,
    ) -> Result<Option<AgentRecord>, Failure> {
        let root = coordinator
            .root_id()
            .ok_or(StorageError::ScopeDenied)?
            .clone();
        let mut after = None;
        let (mut row, mut state) = loop {
            let Some((row, state)) = pending_for_root(tx.git(), &root, None, after)? else {
                return Ok(None);
            };
            after = Some(row.id.clone());
            if coordinator.project_id() != Some(&row.project_id)
                || coordinator.policy_revision().get() != state.revision_of_policy
                || tx.store().restore_epoch != state.restore_epoch
            {
                continue;
            }
            let operation = tx
                .operations()
                .operation(&state.operation)?
                .ok_or(StorageError::ScopeDenied)?;
            if crate::ports::storage::is_terminal(operation.state) {
                continue;
            }
            break (row, state);
        };
        let (tasks, repositories) = match &state.target {
            ReviewTarget::Local { target } => (
                vec![target.task_id.clone()],
                target
                    .diffs
                    .0
                    .iter()
                    .map(|diff| diff.repository_id.clone())
                    .collect(),
            ),
            ReviewTarget::PullRequests { .. } => (
                Vec::new(),
                state
                    .final_rounds
                    .iter()
                    .filter(|round| {
                        round.fallback_requested
                            && !state.value.sources.0.iter().any(|source| {
                                source.repository_id == round.repository_id
                                    && matches!(source.state, ReviewObservationState::Completed)
                            })
                    })
                    .map(|round| round.repository_id.clone())
                    .collect::<Vec<_>>(),
            ),
        };
        let control = crate::services::runs::control_of(tx.runs(), &root)?;
        let child = agents.delegate(
            tx,
            coordinator,
            &crate::operations::agents::DelegateInput {
                root_id: root.clone(),
                expected_control_revision: rev(control.revision),
                task_ids: BoundedList(tasks),
                role_id: Id::new(AgentRole::CodeReviewer.as_str()).expect("reviewer role"),
                instruction: content("Independently review the exact committed task diff."),
                source_checkpoint_id: None,
            },
        )?;
        let assignment = tx
            .runs()
            .assignment(&child.assignment_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let session = tx
            .sessions()
            .session(&assignment.session_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if session.agent_id == state.requested_by {
            return Err(StorageError::ScopeDenied.into());
        }
        state.local_binding = Some(LocalBinding {
            assignment_id: child.assignment_id.clone(),
            session_id: session.id.clone(),
            actor: session.agent_id.clone(),
            repositories: repositories.clone(),
        });
        if let Some(local) = state.value.local_review.as_mut() {
            local.state = ReviewObservationState::Running;
            local.reviewer_actor_id = Some(session.agent_id.0);
            local.reviewer_session_id = Some(session.id.clone());
        } else {
            use crate::protocol::records::ReviewSourceKind;
            for repository in &repositories {
                let source = state
                    .value
                    .sources
                    .0
                    .iter_mut()
                    .find(|source| &source.repository_id == repository)
                    .ok_or(StorageError::ScopeDenied)?;
                if matches!(source.source_kind, ReviewSourceKind::InAppIndependent) {
                    source.state = ReviewObservationState::Running;
                    source.run_id = Some(session.id.clone());
                } else {
                    let mut fallback = source.clone();
                    fallback.source_kind = ReviewSourceKind::InAppFallback;
                    fallback.source_id = session.agent_id.0.clone();
                    fallback.run_id = Some(session.id.clone());
                    fallback.state = ReviewObservationState::Running;
                    fallback.completed_at = None;
                    fallback.finding_count = counter(0);
                    fallback.zero_findings_confirmed = false;
                    fallback.findings_coverage.state =
                        crate::protocol::data::CoverageState::Unavailable;
                    fallback.evidence = BoundedList(Vec::new());
                    state.value.sources.0.push(fallback);
                }
            }
        }
        state.value.state = ReviewState::Running;
        write_local(tx, &mut row, &mut state, coordinator, "CodeReviewStarted")?;
        Ok(Some(child))
    }

    /// Trusted runner input for this independently admitted child, revalidated on every reload.
    pub fn local_input(
        &self,
        snapshot: &dyn ReadSnapshot,
        ctx: &InvocationContext,
    ) -> Result<Option<LocalReviewInput>, StorageError> {
        let Some((root, assignment, session)) = ctx
            .root_id()
            .zip(ctx.assignment_id())
            .zip(ctx.session_id())
            .map(|((root, assignment), session)| (root, assignment, session))
        else {
            return Ok(None);
        };
        if ctx.principal_kind() != PrincipalKind::Agent
            || !snapshot
                .runs()
                .agent_assignment_context(assignment)?
                .is_some_and(|owner| owner.role_id.as_str() == AgentRole::CodeReviewer.as_str())
        {
            return Ok(None);
        }
        let found = pending_for_root(
            snapshot.git(),
            root,
            Some((assignment, session, ctx.principal())),
            None,
        )
        .map_err(Failure::into_storage)?;
        let Some((row, state)) = found else {
            return Ok(None);
        };
        if ctx.project_id() != Some(&row.project_id)
            || ctx.restore_epoch().get() != state.restore_epoch
            || ctx.policy_revision().get() != state.revision_of_policy
            || !snapshot
                .runs()
                .agent_assignment_context(assignment)?
                .is_some_and(|owner| {
                    owner.role_id.as_str() == AgentRole::CodeReviewer.as_str()
                        && owner.project_id == row.project_id
                })
        {
            return Err(StorageError::ScopeDenied);
        }
        Ok(Some(LocalReviewInput {
            review_id: row.id,
            diff_digest: state.value.head_digest.0,
            source_artifacts: state
                .source_artifacts
                .into_iter()
                .filter(|source| {
                    state
                        .local_binding
                        .as_ref()
                        .is_some_and(|binding| binding.repositories.contains(&source.repository_id))
                })
                .collect(),
        }))
    }

    /// Publish one attributable result. Empty findings must arrive in an explicit completed report.
    pub fn complete_local(
        &self,
        ctx: &InvocationContext,
        review_id: &Id,
        report: CodeReviewReport,
    ) -> Result<(), StorageError> {
        if report.findings.len() > 128
            || report.findings.iter().any(|finding| {
                finding.summary.trim().is_empty()
                    || finding.summary.len() > 4096
                    || finding.location.trim().is_empty()
                    || finding.location.len() > 1024
            })
        {
            return Err(StorageError::InvalidRecord(
                "invalid code review findings".into(),
            ));
        }
        let input = read_snapshot(self.store.as_ref(), &ReadOptions::default(), |snapshot| {
            self.local_input(snapshot, ctx)
        })?
        .ok_or(StorageError::ScopeDenied)?;
        if &input.review_id != review_id {
            return Err(StorageError::ScopeDenied);
        }
        let bytes = serde_json::to_vec(&report)
            .map_err(|_| StorageError::InvalidRecord("invalid code review report".into()))?;
        let staged = self
            .artifacts
            .stage_host_bytes(
                self.store.as_ref(),
                &bytes,
                json!({"kind":"CodeReviewReport","reviewId":review_id}),
            )?
            .map_err(StorageError::InvalidRecord)?;
        let artifacts = self.artifacts.clone();
        let ctx = ctx.clone();
        let review_id = review_id.clone();
        run_write(self.store.as_ref(), WriteOptions::settlement(), move |tx| {
            let mut row = tx
                .git()
                .code_review(&review_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let mut state: ReviewStateRecord = decode(&row.body).map_err(Failure::into_storage)?;
            if !matches!(state.value.state, ReviewState::Running) {
                return Ok(());
            }
            if ctx.project_id() != Some(&row.project_id)
                || ctx.root_id() != Some(&row.root_id)
                || ctx.restore_epoch().get() != state.restore_epoch
                || tx.store().restore_epoch != state.restore_epoch
                || ctx.policy_revision().get() != state.revision_of_policy
                || !state.local_binding.as_ref().is_some_and(|binding| {
                    ctx.assignment_id() == Some(&binding.assignment_id)
                        && ctx.session_id() == Some(&binding.session_id)
                        && ctx.principal() == &binding.actor
                })
            {
                return Err(StorageError::ScopeDenied);
            }
            let version = artifacts.commit_host_bytes_in_scope(
                tx,
                &staged,
                HostArtifactPublication {
                    title: "Independent code review",
                    media_type: "application/json",
                    provenance: json!({"reviewId":review_id,"diffDigest":state.value.head_digest,"reason":state.failure}),
                    scope: ArtifactScope::Project(row.project_id.clone()),
                    owner: ctx.principal().clone(),
                },
            )?;
            let evidence = version_ref(&version);
            let counted = report.findings.len();
            let binding = state
                .local_binding
                .as_ref()
                .ok_or(StorageError::ScopeDenied)?;
            let mut counts = std::collections::HashMap::<Id, i64>::new();
            let actionable = report
                .findings
                .iter()
                .any(|finding| !matches!(finding.severity, CodeReviewSeverity::Informational));
            for result in report.findings {
                let repository = match result.repository_id.as_ref() {
                    Some(repo) if binding.repositories.contains(repo) => Some(repo.clone()),
                    None if binding.repositories.len() == 1 => {
                        binding.repositories.first().cloned()
                    }
                    None if state.value.local_review.is_some() => None,
                    _ => {
                        return Err(StorageError::InvalidRecord(
                            "finding lacks its exact repository".into(),
                        ));
                    }
                };
                if let Some(repo) = repository.as_ref() {
                    *counts.entry(repo.clone()).or_default() += 1;
                }
                let id = tx.new_id(IdKind::Link);
                let severity = match result.severity {
                    CodeReviewSeverity::Blocker | CodeReviewSeverity::Major => Severity::Error,
                    CodeReviewSeverity::Minor => Severity::Warning,
                    CodeReviewSeverity::Informational => Severity::Info,
                };
                let value = ReviewFinding {
                    id: id.clone(),
                    review_id: review_id.clone(),
                    revision: rev(1),
                    summary: content(&result.summary),
                    location: Some(BoundedText(result.location)),
                    severity,
                    actionable: !matches!(result.severity, CodeReviewSeverity::Informational),
                    response: None,
                    evidence: BoundedList(vec![evidence.clone()]),
                    code_severity: Some(result.severity),
                    resolution: None,
                };
                let body = serde_json::to_value(&value)
                    .map_err(|_| StorageError::InvalidRecord("invalid finding body".into()))?;
                tx.git().put_code_finding(
                    &StoredCodeFinding {
                        id: id.clone(),
                        review_id: review_id.clone(),
                        repository_id: repository,
                        source_observation_id: None,
                        project_id: row.project_id.clone(),
                        root_id: row.root_id.clone(),
                        revision: 1,
                        body,
                    },
                    None,
                )?;
                state.value.finding_ids.0.push(id);
            }
            if let Some(local) = state.value.local_review.as_mut() {
                local.state = ReviewObservationState::Completed;
                local.completed_at = Some(counter(tx.now_ms()));
                local.report = Some(evidence);
                local.finding_count = counter(counted as i64);
                state.value.state = if actionable {
                    ReviewState::ChangesRequired
                } else {
                    ReviewState::Ready
                };
                write_local(tx, &mut row, &mut state, &ctx, "CodeReviewCompleted")
            } else {
                for source in state
                    .value
                    .sources
                    .0
                    .iter_mut()
                    .filter(|source| source.run_id.as_ref() == Some(&binding.session_id))
                {
                    let count = counts
                        .get(&source.repository_id)
                        .copied()
                        .unwrap_or_default();
                    source.state = ReviewObservationState::Completed;
                    source.completed_at = Some(counter(tx.now_ms()));
                    source.finding_count = counter(count);
                    source.zero_findings_confirmed = count == 0;
                    source.evidence = BoundedList(vec![evidence.clone()]);
                    source.findings_coverage.state = crate::protocol::data::CoverageState::Complete;
                }
                super::remote::update_final_state(tx, &mut row, &mut state)
            }
        })
    }
    pub(super) fn reconcile_cancelled_reviewer(
        &self,
        operation: &crate::ports::storage::OperationRecord,
    ) -> Result<(), StorageError> {
        let id = serde_json::from_value::<Id>(operation.payload["reviewId"].clone())
            .map_err(|_| StorageError::ScopeDenied)?;
        let cancelled = |snapshot: &dyn ReadSnapshot| {
            let row = snapshot
                .git()
                .code_review(&id)?
                .ok_or(StorageError::ScopeDenied)?;
            let state: ReviewStateRecord = decode(&row.body).map_err(Failure::into_storage)?;
            if state.operation != operation.id || !matches!(state.value.state, ReviewState::Running)
            {
                return Ok(None);
            }
            let Some(binding) = state.local_binding.as_ref() else {
                return Ok(None);
            };
            Ok(snapshot
                .runs()
                .assignment(&binding.assignment_id)?
                .filter(|assignment| !assignment.state.is_live())
                .map(|_| row.revision))
        };
        let Some(expected) =
            read_snapshot(self.store.as_ref(), &ReadOptions::default(), cancelled)?
        else {
            return Ok(());
        };
        run_write(self.store.as_ref(), WriteOptions::settlement(), move |tx| {
            let mut row = tx
                .git()
                .code_review(&id)?
                .ok_or(StorageError::ScopeDenied)?;
            if row.revision != expected {
                return Ok(());
            }
            let mut state: ReviewStateRecord = decode(&row.body).map_err(Failure::into_storage)?;
            state.failure =
                Some("The independent review assignment ended before reporting.".into());
            state.value.state = ReviewState::Blocked;
            if let Some(local) = state.value.local_review.as_mut() {
                local.state = ReviewObservationState::Failed;
            }
            if let Some(binding) = state.local_binding.as_ref() {
                for source in &mut state.value.sources.0 {
                    if source.run_id.as_ref() == Some(&binding.session_id)
                        && matches!(source.state, ReviewObservationState::Running)
                    {
                        source.state = ReviewObservationState::Failed;
                    }
                }
            }
            let store = tx.store();
            let host = InvocationContext::new(
                store.host_actor.clone(),
                PrincipalKind::Host,
                id,
                Id::new("code-review").expect("static ingress"),
                store.store_id.clone(),
                counter(store.restore_epoch),
                rev(state.revision_of_policy),
            );
            write_local(tx, &mut row, &mut state, &host, "CodeReviewAssignmentEnded")
        })
    }

    /// Record the reason the bound child could not complete; unrelated work may proceed.
    pub fn fail_local(&self, ctx: &InvocationContext, reason: &str) -> Result<(), StorageError> {
        let Some(input) =
            read_snapshot(self.store.as_ref(), &ReadOptions::default(), |snapshot| {
                self.local_input(snapshot, ctx)
            })?
        else {
            return Ok(());
        };
        let ctx = ctx.clone();
        let reason = reason.chars().take(1024).collect::<String>();
        run_write(self.store.as_ref(), WriteOptions::settlement(), move |tx| {
            let mut row = tx
                .git()
                .code_review(&input.review_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let mut state: ReviewStateRecord = decode(&row.body).map_err(Failure::into_storage)?;
            if !matches!(state.value.state, ReviewState::Running) {
                return Ok(());
            }
            if ctx.project_id() != Some(&row.project_id)
                || ctx.root_id() != Some(&row.root_id)
                || tx.store().restore_epoch != state.restore_epoch
                || !state.local_binding.as_ref().is_some_and(|binding| {
                    ctx.assignment_id() == Some(&binding.assignment_id)
                        && ctx.session_id() == Some(&binding.session_id)
                        && ctx.principal() == &binding.actor
                })
            {
                return Err(StorageError::ScopeDenied);
            }
            state.failure = Some(reason);
            state.value.state = ReviewState::Blocked;
            if let Some(local) = state.value.local_review.as_mut() {
                local.state = ReviewObservationState::Failed;
            } else {
                for source in state
                    .value
                    .sources
                    .0
                    .iter_mut()
                    .filter(|source| source.run_id.as_ref() == ctx.session_id())
                {
                    source.state = ReviewObservationState::Failed;
                }
            }
            if !state.final_rounds.is_empty()
                && state.final_rounds.iter().all(|round| {
                    state.value.sources.0.iter().any(|source| {
                        source.repository_id == round.repository_id
                            && matches!(source.state, ReviewObservationState::Completed)
                    })
                })
            {
                return super::remote::update_final_state(tx, &mut row, &mut state);
            }
            write_local(tx, &mut row, &mut state, &ctx, "CodeReviewFailed")
        })
    }
}

pub(super) fn write_local(
    tx: &mut dyn WriteTransaction,
    row: &mut StoredCodeReview,
    state: &mut ReviewStateRecord,
    ctx: &InvocationContext,
    kind: &'static str,
) -> Result<(), StorageError> {
    let prior = row.revision;
    row.revision = prior
        .checked_add(1)
        .ok_or_else(|| StorageError::InvalidRecord("review revision overflow".into()))?;
    state.value.revision = rev(row.revision);
    row.body = serde_json::to_value(&state)
        .map_err(|_| StorageError::InvalidRecord("invalid local review body".into()))?;
    row.state = row.body["value"]["state"]
        .as_str()
        .ok_or(StorageError::ScopeDenied)?
        .to_owned();
    tx.git().put_code_review(row, Some(prior))?;
    let event = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event,
        actor: ctx.principal().clone(),
        project_id: Some(row.project_id.clone()),
        root_id: Some(row.root_id.clone()),
        operation_id: Some(state.operation.clone()),
        aggregate_kind: "code_review".into(),
        aggregate_id: row.id.as_str().into(),
        aggregate_revision: row.revision,
        kind: kind.into(),
        payload: json!({"reviewId":row.id,"diffDigest":state.value.head_digest,"reason":state.failure}),
    })?;
    Ok(())
}
