//! Host-owned independent review evidence, recorded beside feature readiness.
use super::model::{ActivePlan, FEATURE_UNIT, plan_units};
use super::readiness::Slots;
use crate::context::{InvocationContext, PrincipalKind};
pub use crate::kernel::work_plan::{
    PlanReview, ReviewOutcome, ReviewTarget, target_for_assignment,
};
use crate::kernel::work_plan::{same_plan, targets};
use crate::ports::IdKind;
use crate::ports::storage::{
    IndexRead, NewEvent, StorageError, StoredReadiness, WorkRead, WriteTransaction,
};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::PlanDraft;
use crate::protocol::{AssignmentId, EventId, Id};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Report {
    pub needs_source: bool,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Finding {
    pub concern: String,
    pub consequence: String,
    pub acceptance_case: String,
}

pub fn finding_id(session: &Id, index: usize) -> Id {
    let digest = crate::fingerprint::sha256_hex(format!("{}/{index}", session.as_str()).as_bytes());
    Id::new(format!("prf-{}", &digest[..32])).expect("finding identifier")
}

pub(crate) fn substantive(draft: &PlanDraft) -> bool {
    draft.content.byte_length.get() > 0
        || !draft.repository_ids.0.is_empty()
        || !draft.goals.0.is_empty()
        || draft
            .requirements
            .0
            .iter()
            .any(|r| !super::model::dropped(r))
        || !draft.acceptance_tests.0.is_empty()
        || !draft.acceptance.0.is_empty()
}

/// The exact published view IDs are immutable. Only a report that needed source evidence
/// depends on those views remaining current.
pub fn published_views(
    index: &dyn IndexRead,
    repositories: &[Id],
) -> Result<Vec<Id>, StorageError> {
    let mut views = Vec::new();
    for repository in repositories {
        if let Some(view) = index.published(repository)? {
            views.push(view.view_id);
        }
    }
    views.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(views)
}

pub(crate) fn current_sources(review: &PlanReview, published: Option<&[Id]>) -> bool {
    !review.source_phase || published.is_none_or(|views| review.source_views == views)
}

/// Reuse valid reports and in-flight attempts owned by the current launch.
pub fn next_target(
    tx: &mut dyn WriteTransaction,
    feature: &Id,
    activation: &Id,
    owner_epoch: i64,
) -> Result<Option<(ReviewTarget, Option<AssignmentId>)>, StorageError> {
    let slots = tx
        .work()
        .readiness(feature)?
        .map(|r| Slots::of(&r.body))
        .unwrap_or_default();
    for target in targets(tx.work(), feature)? {
        if !substantive(&target.draft)
            || (!target.required
                && slots.plan_reviews.is_empty()
                && slots.active.as_ref().is_some_and(|a| {
                    a.plan_digest == target.digest && a.units == plan_units(&target.draft)
                }))
        {
            continue;
        }
        let reviews = &slots.plan_reviews;
        let views = if reviews.iter().any(|review| review.source_phase) {
            published_views(tx.indexing(), &target.draft.repository_ids.0)?
        } else {
            Vec::new()
        };
        let units = plan_units(&target.draft);
        if reviews.iter().any(|review| {
            review.units == units
                && current_sources(review, Some(&views))
                && dispositions_recorded(&slots, review, target.pending)
        }) {
            continue;
        }
        if let Some(review) = for_plan(&slots, target.revision, &target.digest) {
            match review.outcome {
                ReviewOutcome::Complete { .. } => {
                    if current_sources(review, Some(&views)) {
                        continue;
                    }
                }
                _ if review.activation == *activation && review.owner_epoch == owner_epoch => {
                    return Ok(Some((target, Some(review.assignment.clone()))));
                }
                _ => {}
            }
        }
        return Ok(Some((target, None)));
    }
    Ok(None)
}

/// Recovery can rebind a durable child to a new launch. An independent review
/// attempt is intentionally single-launch and uses a fresh session on retry.
pub fn owns_attempt(
    read: &dyn WorkRead,
    root: &Id,
    assignment: &AssignmentId,
    session: &Id,
    activation: &Id,
    owner_epoch: i64,
) -> Result<bool, StorageError> {
    Ok(
        target_for_assignment(read, root, assignment)?.is_some_and(|(review, _)| {
            review.session == *session
                && review.activation == *activation
                && review.owner_epoch == owner_epoch
                && matches!(review.outcome, ReviewOutcome::Running)
        }),
    )
}

/// Attach an isolated child in the same transaction that creates its session. A retry replaces
/// the unfinished attempt only when the supervisor owns a new activation.
pub fn attach(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    target: &ReviewTarget,
    assignment: AssignmentId,
    session: Id,
) -> Result<(), StorageError> {
    let (Some(root), Some((activation, owner_epoch))) = (ctx.root_id(), ctx.activation()) else {
        return Err(StorageError::ScopeDenied);
    };
    let existing = tx.work().readiness(&root.0)?;
    let mut slots = existing
        .as_ref()
        .map(|r| Slots::of(&r.body))
        .unwrap_or_default();
    let current = targets(tx.work(), &root.0)?;
    let source_views = published_views(tx.indexing(), &target.draft.repository_ids.0)?;
    if !current
        .iter()
        .any(|t| t.digest == target.digest && t.revision == target.revision)
        || slots.plan_reviews.iter().any(|r| {
            same_plan(r, target)
                && match r.outcome {
                    ReviewOutcome::Complete { .. } => current_sources(r, Some(&source_views)),
                    _ => r.activation == *activation && r.owner_epoch == owner_epoch,
                }
        })
    {
        return Err(StorageError::ScopeDenied);
    }
    // Keep exact evidence for the live slots and the previous completed review, which can
    // still cover unchanged requirements after adoption of a changed plan.
    let previous_completed = slots
        .plan_reviews
        .iter()
        .rposition(|r| matches!(r.outcome, ReviewOutcome::Complete { .. }))
        .map(|index| slots.plan_reviews[index].session.clone());
    slots.plan_reviews.retain(|review| {
        !same_plan(review, target)
            && (current.iter().any(|t| same_plan(review, t))
                || previous_completed.as_ref() == Some(&review.session))
    });
    let units = plan_units(&target.draft);
    let affected_units = match previous_completed.as_ref().and_then(|session| {
        slots
            .plan_reviews
            .iter()
            .find(|review| &review.session == session)
    }) {
        Some(previous)
            if previous.units.get(FEATURE_UNIT) == units.get(FEATURE_UNIT)
                && current_sources(previous, Some(&source_views))
                && dispositions_recorded(&slots, previous, target.pending) =>
        {
            units
                .iter()
                .filter(|(key, unit)| previous.units.get(*key) != Some(*unit))
                .map(|(key, _)| key.clone())
                .collect()
        }
        _ => units.keys().cloned().collect(),
    };
    let reviewer = tx
        .sessions()
        .session(&session)?
        .ok_or(StorageError::ScopeDenied)?;
    let review = PlanReview {
        plan_revision: target.revision,
        plan_digest: target.digest.clone(),
        assignment,
        session,
        actor: reviewer.agent_id,
        started_at_ms: reviewer.created_at_ms,
        activation: activation.clone(),
        owner_epoch,
        units,
        affected_units,
        source_views,
        source_phase: false,
        outcome: ReviewOutcome::Running,
    };
    slots.plan_reviews.push(review.clone());
    save(
        tx,
        ctx,
        &root.0,
        existing.as_ref(),
        &slots,
        &review,
        "PlanReviewStarted",
    )
}

pub fn begin_source_phase(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
) -> Result<bool, StorageError> {
    change(tx, ctx, |review| {
        if review.source_phase || review.source_views.is_empty() {
            return None;
        }
        review.source_phase = true;
        Some("PlanReviewSourcesEnabled")
    })
}

/// The child owns only its own review result. Superseded plans, assignments and activations
/// cannot settle a newer attempt.
pub fn settle(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    outcome: ReviewOutcome,
) -> Result<bool, StorageError> {
    if matches!(outcome, ReviewOutcome::Running) {
        return Err(StorageError::ScopeDenied);
    }
    if matches!(outcome, ReviewOutcome::Complete { .. })
        && let (Some(root), Some(assignment)) = (ctx.root_id(), ctx.assignment_id())
        && let Some((review, target)) = target_for_assignment(tx.work(), &root.0, assignment)?
        && review.source_phase
        && review.source_views != published_views(tx.indexing(), &target.draft.repository_ids.0)?
    {
        return Err(StorageError::InvalidRecord(
            "the published source changed during review; retry against the current source".into(),
        ));
    }
    change(tx, ctx, |review| {
        review.outcome = outcome;
        Some("PlanReviewSettled")
    })
}

fn change(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    update: impl FnOnce(&mut PlanReview) -> Option<&'static str>,
) -> Result<bool, StorageError> {
    let (Some(root), Some(assignment), Some(session), Some((activation, owner_epoch))) = (
        ctx.root_id(),
        ctx.assignment_id(),
        ctx.session_id(),
        ctx.activation(),
    ) else {
        return Err(StorageError::ScopeDenied);
    };
    if ctx.principal_kind() != PrincipalKind::Agent
        || tx
            .authority()
            .principal(ctx.principal())?
            .and_then(|p| p.role)
            != Some(crate::settings::catalog::AgentRole::PlanReviewer)
        || tx.runs().activation(activation)?.is_none_or(|a| {
            a.root_id != *root
                || a.owner_epoch != owner_epoch
                || a.phase != crate::ports::storage::ActivationPhase::Running
        })
        || tx
            .sessions()
            .session(session)?
            .is_none_or(|s| s.closed() || s.agent_id != *ctx.principal())
        || tx.runs().assignment(assignment)?.is_none_or(|a| {
            a.execution_id.as_ref() != Some(activation)
                || a.state != crate::ports::storage::AssignmentState::Active
        })
    {
        return Err(StorageError::ScopeDenied);
    }
    let Some(existing) = tx.work().readiness(&root.0)? else {
        return Ok(false);
    };
    let mut slots = Slots::of(&existing.body);
    let live_targets = targets(tx.work(), &root.0)?;
    let Some(review) = slots.plan_reviews.iter_mut().find(|review| {
        &review.assignment == assignment
            && &review.session == session
            && &review.activation == activation
            && review.owner_epoch == owner_epoch
            && matches!(review.outcome, ReviewOutcome::Running)
            && live_targets.iter().any(|target| same_plan(review, target))
    }) else {
        return Ok(false);
    };
    let Some(event) = update(review) else {
        return Ok(false);
    };
    let review = review.clone();
    if matches!(review.outcome, ReviewOutcome::Complete { findings, .. } if findings > 0) {
        for assessment in [&mut slots.active, &mut slots.pending]
            .into_iter()
            .flatten()
        {
            if assessment.plan_digest == review.plan_digest {
                // Reassess the scope in the light of the report. Keep prior findings and their
                // dispositions so an independent review cannot silently clear an existing hold.
                for key in &review.affected_units {
                    assessment.units.remove(key);
                }
            }
        }
    }
    save(tx, ctx, &root.0, Some(&existing), &slots, &review, event)?;
    Ok(true)
}

fn save(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    feature: &Id,
    previous: Option<&StoredReadiness>,
    slots: &Slots,
    review: &PlanReview,
    event: &'static str,
) -> Result<(), StorageError> {
    let expected = previous.map(|r| r.revision);
    let revision = expected.unwrap_or(0) + 1;
    tx.work().put_readiness(
        &StoredReadiness {
            feature_id: feature.clone(),
            revision,
            body: slots.value(),
        },
        expected,
    )?;
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: ctx.principal().clone(),
        project_id: ctx.project_id().cloned(),
        root_id: ctx.root_id().cloned(),
        operation_id: None,
        aggregate_kind: super::readiness::READINESS_AGGREGATE.into(),
        aggregate_id: feature.as_str().into(),
        aggregate_revision: revision,
        kind: event.into(),
        payload: json!(review),
    })?;
    Ok(())
}

pub(crate) fn review_holds(
    slots: &Slots,
    active: &ActivePlan,
    key: &str,
    digest: &str,
    global: &str,
    published_views: Option<&[Id]>,
) -> bool {
    if !substantive(&active.draft) || (!active.review_required && slots.plan_reviews.is_empty()) {
        return false;
    }
    if let Some(review) = for_plan(slots, active.plan_revision, &active.digest)
        && matches!(review.outcome, ReviewOutcome::Complete { .. })
        && current_sources(review, published_views)
    {
        // An adopted revision can carry an assessment written for its predecessor. Matching
        // scope hashes cannot release affected work before this report is dispositioned in
        // the active assessment, irrespective of that assessment's original plan digest.
        return review.affected_units.iter().any(|unit| unit == key)
            && !dispositions_recorded(slots, review, false);
    }
    // Starting review of a pending replacement leaves an already accepted legacy plan active.
    if !active.review_required
        && slots.active.as_ref().is_some_and(|a| {
            a.plan_digest == active.digest && a.units.get(key).is_some_and(|unit| unit == digest)
        })
        && !slots
            .plan_reviews
            .iter()
            .any(|r| r.plan_revision <= active.plan_revision)
    {
        return false;
    }
    !slots.plan_reviews.iter().any(|review| {
        dispositions_recorded(slots, review, false)
            && current_sources(review, published_views)
            && review
                .units
                .get(FEATURE_UNIT)
                .is_some_and(|unit| unit == global)
            && review.units.get(key).is_some_and(|old| old == digest)
    })
}

pub(crate) fn report_matches(evidence: &ArtifactRef, report: &ArtifactRef) -> bool {
    evidence.version_id == report.version_id
        && evidence.artifact_id == report.artifact_id
        && evidence.sha256 == report.sha256
        && evidence.byte_length == report.byte_length
        && evidence.media_type == report.media_type
}

fn dispositions_recorded(slots: &Slots, review: &PlanReview, allow_pending: bool) -> bool {
    let ReviewOutcome::Complete { report, findings } = &review.outcome else {
        return false;
    };
    (0..*findings).all(|index| {
        let id = finding_id(&review.session, index);
        [(false, &slots.active), (true, &slots.pending)]
            .into_iter()
            .filter(|(pending, _)| !pending || allow_pending)
            .any(|(pending, assessment)| {
                assessment
                    .iter()
                    .flat_map(|assessment| &assessment.findings)
                    .chain(slots.archived_finding(&id, pending))
                    .any(|finding| {
                        finding.id == id
                            && finding
                                .evidence
                                .0
                                .iter()
                                .any(|evidence| report_matches(evidence, report))
                    })
            })
    })
}

pub(crate) fn for_plan<'a>(
    slots: &'a Slots,
    revision: i64,
    digest: &str,
) -> Option<&'a PlanReview> {
    slots
        .plan_reviews
        .iter()
        .find(|r| r.plan_revision == revision && r.plan_digest == digest)
}

impl PlanReview {
    pub(crate) fn summary(&self) -> String {
        match &self.outcome {
            ReviewOutcome::Running => "Independent plan review is running.".into(),
            ReviewOutcome::Failed { reason } => {
                format!("Independent plan review failed. {reason} Start or Continue to retry.")
            }
            ReviewOutcome::Complete { report, findings } => format!(
                "Independent plan review completed with {findings} material findings. Report {} at version {}.",
                report.artifact_id.as_str(),
                report.version_id.as_str()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ActorId;
    use crate::services::work::{model::plan_digest, readiness::Assessment};

    fn id(value: &str) -> Id {
        Id::new(value).unwrap()
    }

    #[test]
    fn a_changed_requirement_holds_its_scope_and_a_changed_contract_holds_shared_scope() {
        let mut plan: PlanDraft = serde_json::from_value(json!({
            "content": {"artifactId": "plan", "versionId": "plan-version", "mediaType": "text/plain",
                "sha256": "a".repeat(64), "byteLength": "100"},
            "repositoryIds": [], "acceptance": [], "acceptanceTests": [], "goals": [],
            "requirements": [
                {"id": "E1", "statement": "Writes use the old contract", "status": "Confirmed",
                    "coveredByTaskIds": []},
                {"id": "E2", "statement": "Reads are independent", "status": "Confirmed",
                    "coveredByTaskIds": []}
            ]
        })).unwrap();
        let before = plan_units(&plan);
        let digest = plan_digest(&plan);
        let reviewer = ActorId(id("reviewer"));
        let slots = Slots {
            active: Some(Assessment {
                id: id("assessed"),
                plan_revision: 1,
                plan_digest: digest.clone(),
                reviewed_pending: false,
                units: before.clone(),
                summary: "Ready".into(),
                findings: vec![],
                assessed_by: ActorId(id("coordinator")),
                assessed_at_ms: 0,
            }),
            plan_reviews: vec![PlanReview {
                plan_revision: 1,
                plan_digest: digest,
                assignment: AssignmentId(id("review")),
                session: id("review-session"),
                actor: reviewer,
                started_at_ms: 0,
                activation: id("activation"),
                owner_epoch: 1,
                units: before.clone(),
                affected_units: before.keys().cloned().collect(),
                source_views: vec![],
                source_phase: false,
                outcome: ReviewOutcome::Complete {
                    report: plan.content.clone(),
                    findings: 0,
                },
            }],
            ..Slots::default()
        };
        plan.requirements.0[0].statement.0 = "Writes use the revised contract".into();
        let changed = ActivePlan {
            plan_revision: 2,
            digest: plan_digest(&plan),
            draft: plan,
            proposal_id: id("new-approval"),
            review_required: true,
        };
        let readiness = Some((1, slots.value()));
        let scope = |plan: &ActivePlan| {
            super::super::readiness::scope_plan(
                &serde_json::to_value(plan).unwrap(),
                None,
                readiness.as_ref(),
                &[],
            )
            .unwrap()
        };
        let changed_scope = scope(&changed);
        assert!(
            !changed_scope
                .unit(Some("E1"))
                .unwrap()
                .readiness
                .is_current()
        );
        assert!(
            changed_scope
                .unit(Some("E2"))
                .unwrap()
                .readiness
                .is_current()
        );

        // A revision that only changed its budget retains the previous scope hashes. A
        // fresh review after source drift can return findings while that assessment is still
        // stored under the old digest. Only its affected units must wait for disposition.
        let mut carried = slots.clone();
        let successor = ActivePlan {
            plan_revision: 2,
            digest: "c".repeat(64),
            draft: {
                let mut draft = changed.draft.clone();
                draft.requirements.0[0].statement.0 = "Writes use the old contract".into();
                draft
            },
            proposal_id: id("budget-only-approval"),
            review_required: true,
        };
        let review = &mut carried.plan_reviews[0];
        review.plan_revision = successor.plan_revision;
        review.plan_digest.clone_from(&successor.digest);
        review.source_phase = true;
        review.source_views = vec![id("new-source")];
        review.affected_units = vec!["E1".into()];
        review.outcome = ReviewOutcome::Complete {
            report: successor.draft.content.clone(),
            findings: 1,
        };
        let source_session = review.session.clone();
        let carried_scope = |slots: &Slots| {
            super::super::readiness::scope_plan(
                &serde_json::to_value(&successor).unwrap(),
                None,
                Some(&(2, slots.value())),
                &[id("new-source")],
            )
            .unwrap()
        };
        assert!(
            !carried_scope(&carried)
                .unit(Some("E1"))
                .unwrap()
                .readiness
                .is_current()
        );
        assert!(
            carried_scope(&carried)
                .unit(Some("E2"))
                .unwrap()
                .readiness
                .is_current()
        );

        let disposition = serde_json::from_value(json!({
            "id": finding_id(&source_session, 0), "disposition": "ResolvedDecision",
            "state": "Resolved", "blocking": false, "prerequisite": false,
            "requirementIds": ["E1"], "affectedWorkIds": [],
            "summary": "Verified the change", "missing": "", "consequence": "",
            "attempts": [], "insufficiency": "", "closureCondition": "",
            "decision": "The contract remains valid", "confidence": "High",
            "evidence": [successor.draft.content], "closureEvidence": []
        }))
        .unwrap();
        let mut pending = carried.active.clone().unwrap();
        pending.findings.push(disposition);
        carried.pending = Some(pending.clone());
        assert!(
            !carried_scope(&carried)
                .unit(Some("E1"))
                .unwrap()
                .readiness
                .is_current()
        );
        carried.active = Some(pending);
        assert!(
            carried_scope(&carried)
                .unit(Some("E1"))
                .unwrap()
                .readiness
                .is_current()
        );

        let disposition = carried.active.as_mut().unwrap().findings.pop().unwrap();
        carried.pending = None;
        let archived = json!({
            disposition.id.as_str(): {
                "planRevision": successor.plan_revision,
                "planDigest": successor.digest,
                "finding": disposition
            }
        });
        let mut stored = carried.value();
        stored["pendingArchive"] = archived.clone();
        assert!(
            !carried_scope(&Slots::of(&stored))
                .unit(Some("E1"))
                .unwrap()
                .readiness
                .is_current()
        );
        stored["activeArchive"] = archived;
        assert!(
            carried_scope(&Slots::of(&stored))
                .unit(Some("E1"))
                .unwrap()
                .readiness
                .is_current()
        );

        let mut changed = changed;
        changed.draft.content.sha256 = "b".repeat(64).try_into().unwrap();
        changed.digest = plan_digest(&changed.draft);
        let shared_change = scope(&changed);
        assert!(
            !shared_change
                .unit(Some("E2"))
                .unwrap()
                .readiness
                .is_current()
        );
    }
}
