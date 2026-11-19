//! Stored plans and independent review evidence shared by work and authority.
use crate::context::InvocationContext;
use crate::ports::storage::{StorageError, WorkRead};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::PlanDraft;
use crate::protocol::{ActorId, AssignmentId, Id};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The active approved plan as the slot stores it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivePlan {
    pub draft: PlanDraft,
    pub digest: String,
    /// The slot revision of the pending draft that was approved.
    pub plan_revision: i64,
    pub proposal_id: Id,
    #[serde(default)]
    pub review_required: bool,
}
/// The pending revision as the slot stores it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingPlan {
    pub draft: PlanDraft,
    pub digest: String,
    #[serde(default)]
    pub review_required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<crate::protocol::records::FeatureBudgetInput>,
}
pub fn active_of(v: Option<&Value>) -> Option<ActivePlan> {
    v.and_then(|v| serde_json::from_value(v.clone()).ok())
}
pub fn pending_of(v: Option<&Value>) -> Option<PendingPlan> {
    v.and_then(|v| serde_json::from_value(v.clone()).ok())
}

#[derive(Clone, Debug)]
pub struct ReviewTarget {
    pub revision: i64,
    pub digest: String,
    pub draft: PlanDraft,
    pub(crate) required: bool,
    pub(crate) pending: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanReview {
    pub plan_revision: i64,
    pub plan_digest: String,
    pub assignment: AssignmentId,
    pub session: Id,
    pub actor: ActorId,
    pub started_at_ms: i64,
    pub activation: Id,
    pub owner_epoch: i64,
    pub units: BTreeMap<String, String>,
    pub affected_units: Vec<String>,
    #[serde(default)]
    pub source_views: Vec<Id>,
    #[serde(default)]
    pub source_phase: bool,
    pub outcome: ReviewOutcome,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase")]
pub enum ReviewOutcome {
    Running,
    Failed {
        reason: String,
    },
    Complete {
        report: ArtifactRef,
        findings: usize,
    },
}

pub(crate) fn targets(
    read: &dyn WorkRead,
    feature: &Id,
) -> Result<Vec<ReviewTarget>, StorageError> {
    let Some(slot) = read.feature_plan(feature)? else {
        return Ok(Vec::new());
    };
    Ok(active_of(slot.approved.as_ref())
        .map(|active| ReviewTarget {
            revision: active.plan_revision,
            digest: active.digest,
            draft: active.draft,
            required: active.review_required,
            pending: false,
        })
        .into_iter()
        .chain(
            pending_of(slot.pending.as_ref())
                .zip(slot.pending_revision)
                .map(|(pending, revision)| ReviewTarget {
                    revision,
                    digest: pending.digest,
                    draft: pending.draft,
                    required: pending.review_required,
                    pending: true,
                }),
        )
        .collect())
}

pub(crate) fn same_plan(review: &PlanReview, target: &ReviewTarget) -> bool {
    review.plan_revision == target.revision && review.plan_digest == target.digest
}

pub fn target_for_assignment(
    read: &dyn WorkRead,
    feature: &Id,
    assignment: &AssignmentId,
) -> Result<Option<(PlanReview, ReviewTarget)>, StorageError> {
    let Some(record) = read.readiness(feature)? else {
        return Ok(None);
    };
    let Some(reviews) = record.body.get("planReviews") else {
        return Ok(None);
    };
    let reviews: Vec<PlanReview> = serde_json::from_value(reviews.clone())
        .map_err(|_| StorageError::IncompatibleSchema("stored plan reviews".into()))?;
    let Some(review) = reviews
        .into_iter()
        .find(|review| &review.assignment == assignment)
    else {
        return Ok(None);
    };
    Ok(targets(read, feature)?
        .into_iter()
        .find(|target| same_plan(&review, target))
        .map(|target| (review, target)))
}

/// A review's only source reads address exact, host-captured views of the plan repositories.
pub(crate) fn query_isolated(
    read: &dyn WorkRead,
    ctx: &InvocationContext,
    input: &crate::operations::OperationInput,
) -> Result<bool, StorageError> {
    use crate::operations::{OperationInput, code};
    use crate::protocol::{
        data::{DataScope, GraphQuery, ViewSelection},
        records::CodeSourceSelection,
    };
    let (Some(root), Some(assignment), Some(session)) =
        (ctx.root_id(), ctx.assignment_id(), ctx.session_id())
    else {
        return Ok(false);
    };
    let Some((review, target)) = target_for_assignment(read, &root.0, assignment)? else {
        return Ok(false);
    };
    let OperationInput::Code(code::Input::Source(request)) = input else {
        return Ok(false);
    };
    Ok(review.session == *session
        && review.actor == *ctx.principal()
        && ctx.activation() == Some((&review.activation, review.owner_epoch))
        && review.source_phase
        && matches!(review.outcome, ReviewOutcome::Running)
        && matches!(&request.scope, DataScope::Project { project_id } if Some(project_id) == ctx.project_id())
        && matches!(&request.view, Some(ViewSelection::Exact { view_ids })
            if !view_ids.is_empty() && view_ids.iter().all(|id| review.source_views.contains(id)))
        && matches!(&request.query, GraphQuery::Source { selection: Some(CodeSourceSelection::File { repository_id, .. }), .. }
            if target.draft.repository_ids.0.contains(repository_id))
        && request.current.is_none())
}
