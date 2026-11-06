//! The stored shape of work (the brief, the plan slot bodies) and its wire projection.
use crate::fingerprint::sha256_hex;
use crate::ports::storage::{FEATURE, StorageError, StoredWork, WORKFLOW_QUEUE, WorkRead};
use crate::protocol::data::{Coverage, CoverageState};
use crate::protocol::provider::{ArtifactRef, Digest};
use crate::protocol::records::{
    AcceptanceCriterion, ApprovalState, CriterionState, HandoffRecord, PendingFeaturePlan,
    PlanApproval, PlanDraft, ReadinessAssessment, RequirementItem, RequirementStatus, TouchRef,
    WorkAttempt, WorkComment, WorkItemType, WorkKind, WorkLink, WorkRecord,
};
use crate::protocol::support::{BoundedList, BoundedText, Label};
use crate::protocol::{ActorId, AssignmentId, Counter, Id, OperationState};
use crate::services::sessions::{content, counter, label, rev};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The one case folding of work search, applied to stored search keys and to queries alike:
/// full Unicode lowercase plus the folds lowercase leaves apart (`ß`/`ẞ` → `ss`, final sigma →
/// sigma), so the comparison is symmetric in every script.
pub fn fold(s: &str) -> String {
    s.to_lowercase()
        .replace('ß', "ss")
        .replace('ς', "σ")
        .replace('ﬁ', "fi")
        .replace('ﬂ', "fl")
}
/// The folded search key of a title and description.
pub fn search_key(title: &str, description: &str) -> String {
    let mut k = fold(title);
    k.push('\n');
    k.push_str(&fold(description));
    k.chars().take(200_000).collect()
}

/// A recorded entry with who added it and when (the wire entry carries neither).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Noted<T> {
    pub item: T,
    pub actor: ActorId,
    pub at_ms: i64,
}

/// What a work item holds beyond its indexed columns (`work_items.brief_json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WorkBrief {
    /// Authored task classification, independent of feature membership. Legacy rows stay None.
    pub task_type: Option<WorkItemType>,
    pub labels: Vec<Label>,
    pub description: String,
    pub goals: Vec<String>,
    /// A card's acceptance criteria. For a feature, the recorded results (state and evidence)
    /// of its active plan's acceptance criteria, by ID at their approved text.
    pub acceptance: Vec<AcceptanceCriterion>,
    /// A feature's recorded results of its active plan's acceptance tests.
    pub acceptance_tests: Vec<AcceptanceCriterion>,
    pub inherits: Vec<Id>,
    pub previous_inheritance: Option<crate::protocol::records::WorkInheritanceOrigin>,
    pub touches: Vec<TouchRef>,
    pub links: Vec<WorkLink>,
    pub comments: Vec<Noted<WorkComment>>,
    pub handoffs: Vec<Noted<HandoffRecord>>,
    pub acceptance_report: Option<ArtifactRef>,
    pub assignment_id: Option<AssignmentId>,
    pub assigned_agent_id: Option<Id>,
    pub completed_by: Option<ActorId>,
    pub completed_at_ms: Option<i64>,
}
impl WorkBrief {
    pub fn of(w: &StoredWork) -> Result<Self, StorageError> {
        serde_json::from_value(w.brief.clone())
            .map_err(|e| StorageError::InvalidRecord(format!("stored work brief: {e}")))
    }
    pub fn value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }
}

/// The active approved plan as the slot stores it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivePlan {
    pub draft: PlanDraft,
    pub digest: String,
    /// The slot revision of the pending draft that was approved.
    pub plan_revision: i64,
    pub proposal_id: Id,
}
/// The pending revision as the slot stores it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingPlan {
    pub draft: PlanDraft,
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<crate::protocol::records::FeatureBudgetInput>,
}
pub fn active_of(v: Option<&Value>) -> Option<ActivePlan> {
    v.and_then(|v| serde_json::from_value(v.clone()).ok())
}
pub fn pending_of(v: Option<&Value>) -> Option<PendingPlan> {
    v.and_then(|v| serde_json::from_value(v.clone()).ok())
}

fn digest_of(v: &Value) -> String {
    sha256_hex(
        crate::jcs::canonicalize(v)
            .unwrap_or_else(|_| v.to_string())
            .as_bytes(),
    )
}
/// The binding digest of a plan draft (internal conflict/approval token, not a version).
pub fn plan_digest(d: &PlanDraft) -> String {
    digest_of(&serde_json::to_value(d).unwrap_or(Value::Null))
}
/// A cap proposal is part of the exact reviewed binding, although it is not plan content.
pub fn pending_digest(
    draft: &PlanDraft,
    budget: Option<&crate::protocol::records::FeatureBudgetInput>,
) -> String {
    match budget {
        None => plan_digest(draft),
        Some(budget) => digest_of(&json!({"plan":draft,"budget":budget})),
    }
}

/// The key of the feature-wide scope unit in unit maps (named units are requirement IDs).
pub const FEATURE_UNIT: &str = "";
/// The approved scope units of a plan and the digest of each: the feature-wide unit (content,
/// goals, acceptance, acceptance tests, repositories) and one per live requirement (its
/// statement and status; which tasks cover it is not scope). Readiness evidence of a unit stays
/// current while its digest is unchanged.
pub fn plan_units(d: &PlanDraft) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    out.insert(
        FEATURE_UNIT.to_owned(),
        digest_of(&json!({
            "content": d.content,
            "goals": d.goals,
            "acceptance": d.acceptance,
            "acceptanceTests": d.acceptance_tests,
            "repositoryIds": d.repository_ids,
        })),
    );
    for r in d.requirements.0.iter().filter(|r| !dropped(r)) {
        out.insert(
            r.id.as_str().to_owned(),
            digest_of(&json!({"id": r.id, "statement": r.statement, "status": r.status})),
        );
    }
    out
}
pub fn dropped(r: &RequirementItem) -> bool {
    matches!(r.status, RequirementStatus::Dropped)
}

/// A link's identity for removal: its target ID, or a digest of its URL or artifact.
pub fn link_id(l: &WorkLink) -> Id {
    if let Some(t) = &l.target_id {
        return t.clone();
    }
    let basis = json!({"kind": l.kind, "url": l.url, "artifact": l.artifact});
    Id::new(format!("lnk-{}", &digest_of(&basis)[..32])).expect("derived identifier")
}

/// The active plan's criteria with the results recorded for them: a recorded result applies
/// while its ID and text match the approved criterion; otherwise the plan's own statement
/// stands.
pub fn overlay(
    plan: &[AcceptanceCriterion],
    recorded: &[AcceptanceCriterion],
) -> Vec<AcceptanceCriterion> {
    plan.iter()
        .map(|c| match recorded.iter().find(|r| r.id == c.id) {
            Some(r) if r.text.0 == c.text.0 => r.clone(),
            _ => c.clone(),
        })
        .collect()
}
/// A feature's effective acceptance criteria and acceptance tests: its active plan's, with the
/// results recorded for them (none before its first approval).
pub fn feature_criteria(
    active: Option<&ActivePlan>,
    brief: &WorkBrief,
) -> (Vec<AcceptanceCriterion>, Vec<AcceptanceCriterion>) {
    match active {
        None => (Vec::new(), Vec::new()),
        Some(a) => (
            overlay(&a.draft.acceptance.0, &brief.acceptance),
            overlay(&a.draft.acceptance_tests.0, &brief.acceptance_tests),
        ),
    }
}
/// Whether a feature's active acceptance (criteria and tests together) is verified: at least
/// one, and every one met with evidence.
pub fn feature_verified(active: Option<&ActivePlan>, brief: &WorkBrief) -> bool {
    let (a, t) = feature_criteria(active, brief);
    let all: Vec<AcceptanceCriterion> = a.into_iter().chain(t).collect();
    verified(&all)
}

/// Whether every criterion is met with evidence (and there is at least one).
pub fn verified(criteria: &[AcceptanceCriterion]) -> bool {
    !criteria.is_empty()
        && criteria
            .iter()
            .all(|c| matches!(c.state, CriterionState::Met) && !c.evidence_ids.0.is_empty())
}

/// What a record projection reads besides the stored item.
pub struct Extras {
    pub budget: Option<crate::protocol::records::FeatureBudgetView>,
    /// The root's live activation (its execution ID), if it has one.
    pub live_execution: Option<Id>,
    /// The assessment of the active plan (the readiness that gates work).
    pub readiness: Option<ReadinessAssessment>,
    /// The assessment of the pending replacement draft (gates nothing until adopted).
    pub pending_readiness: Option<ReadinessAssessment>,
    /// The detail page: comment/handoff offset, page size, cursor encoder.
    pub detail: Detail,
}
#[derive(Clone)]
pub enum Detail {
    Summary,
    Full {
        offset: usize,
        limit: usize,
        next: Option<String>,
    },
}

/// Most comments a summary record carries (the newest).
pub const SUMMARY_COMMENTS: usize = 20;
/// Most handoffs a summary record carries (the newest).
pub const SUMMARY_HANDOFFS: usize = 3;

fn window<T: Clone>(all: &[T], detail: &Detail, summary: usize) -> (Vec<T>, bool) {
    match detail {
        Detail::Summary => {
            let start = all.len().saturating_sub(summary);
            (all[start..].to_vec(), start > 0)
        }
        Detail::Full { offset, limit, .. } => {
            let start = (*offset).min(all.len());
            let end = start.saturating_add(*limit).min(all.len());
            (all[start..end].to_vec(), start > 0 || end < all.len())
        }
    }
}

/// The wire record of a work item.
pub fn record(
    read: &dyn WorkRead,
    w: &StoredWork,
    extras: Extras,
) -> Result<WorkRecord, StorageError> {
    let brief = WorkBrief::of(w)?;
    let feature = w.kind == FEATURE;
    let queue_revision = read.queue_revision(&w.project_id, &w.queue())?;
    let workflow_revision = read.queue_revision(&w.project_id, WORKFLOW_QUEUE)?;
    let mut repository_ids = read.work_repositories(&w.id)?;
    repository_ids.truncate(128);
    // Complete: no writer stores more than the bound (a replacement starts from this list).
    let mut dependency_ids = read.prerequisites(&w.id)?;
    dependency_ids.truncate(super::deps::MAX_PREREQUISITES);
    let mut dependent_ids = read.dependents(&w.id)?;
    dependent_ids.truncate(128);
    let (mut plan, mut approval, mut pending_plan) = (None, None, None);
    let (mut goals, mut requirements, mut acceptance) = (
        brief.goals.clone(),
        Vec::new(),
        if feature {
            Vec::new()
        } else {
            brief.acceptance.clone()
        },
    );
    let mut acceptance_tests: Option<Vec<AcceptanceCriterion>> = feature.then(Vec::new);
    if feature && let Some(slot) = read.feature_plan(&w.id)? {
        if let Some(a) = active_of(slot.approved.as_ref()) {
            plan = Some(a.draft.content.clone());
            approval = Digest::try_from(a.digest.clone())
                .ok()
                .map(|d| PlanApproval {
                    proposal_id: a.proposal_id.clone(),
                    plan_revision: rev(a.plan_revision),
                    plan_digest: d,
                    state: ApprovalState::Approved,
                });
            goals = a.draft.goals.0.iter().map(|g| g.0.clone()).collect();
            requirements = a.draft.requirements.0.clone();
            let (criteria, tests) = feature_criteria(Some(&a), &brief);
            acceptance = criteria;
            acceptance_tests = Some(tests);
        }
        if let (Some(p), Some(prev)) = (pending_of(slot.pending.as_ref()), slot.pending_revision) {
            let latest = read.latest_proposal(&w.id)?;
            let approval = latest
                .filter(|l| l.plan_revision == prev && l.plan_digest == p.digest)
                .and_then(|l| {
                    let state = match l.state.as_str() {
                        "Pending" => ApprovalState::Pending,
                        "Rejected" => ApprovalState::Rejected,
                        "Expired" => ApprovalState::Expired,
                        // An approved request was adopted; a superseded one is stale.
                        _ => return None,
                    };
                    Some(PlanApproval {
                        proposal_id: l.id,
                        plan_revision: rev(prev),
                        plan_digest: Digest::try_from(l.plan_digest).ok()?,
                        state,
                    })
                });
            pending_plan = Some(PendingFeaturePlan {
                revision: rev(prev),
                draft: p.draft,
                approval,
            });
        }
    }
    let (comments, more_comments) = window(&brief.comments, &extras.detail, SUMMARY_COMMENTS);
    let (handoffs, more_handoffs) = window(&brief.handoffs, &extras.detail, SUMMARY_HANDOFFS);
    let attempts: Vec<WorkAttempt> = extras
        .live_execution
        .iter()
        .map(|e| WorkAttempt {
            id: e.clone(),
            assignment_id: None,
            state: OperationState::Dispatching,
            git_links: BoundedList(Vec::new()),
            checkpoint: None,
        })
        .collect();
    let truncated = more_comments || more_handoffs;
    let detail_cursor = match &extras.detail {
        Detail::Full { next, .. } => next.clone().map(BoundedText),
        Detail::Summary => None,
    };
    let kind = if feature {
        WorkKind::Feature
    } else if w.parent_feature_id.is_some() {
        WorkKind::Task
    } else {
        WorkKind::Issue
    };
    Ok(WorkRecord {
        id: w.id.clone(),
        revision: rev(w.revision),
        project_id: w.project_id.clone(),
        kind,
        task_type: brief.task_type.clone(),
        labels: Some(BoundedList(brief.labels.clone())),
        title: label(&w.title, "work"),
        description: content(&brief.description),
        status_id: w
            .status_id
            .clone()
            .unwrap_or_else(|| Id::new("unset").expect("identifier")),
        priority: i32::try_from(w.priority).unwrap_or(0),
        repository_ids: BoundedList(repository_ids),
        parent_id: w.parent_feature_id.clone(),
        plan,
        approval,
        dependency_ids: BoundedList(dependency_ids),
        assigned_agent_id: brief.assigned_agent_id.clone(),
        caller_ref: w.caller_ref.clone(),
        goals: BoundedList(goals.iter().take(128).map(|g| content(g)).collect()),
        requirements: BoundedList(requirements.into_iter().take(128).collect()),
        acceptance: BoundedList(acceptance.into_iter().take(128).collect()),
        inherits_requirement_ids: BoundedList(brief.inherits.iter().take(128).cloned().collect()),
        previous_inheritance: brief.previous_inheritance.clone(),
        budget: if feature { extras.budget } else { None },
        touches: BoundedList(brief.touches.iter().take(128).cloned().collect()),
        links: BoundedList(brief.links.iter().take(128).cloned().collect()),
        comments: BoundedList(comments.into_iter().map(|c| c.item).collect()),
        handoffs: BoundedList(handoffs.into_iter().map(|h| h.item).collect()),
        attempt_count: counter(attempts.len() as i64),
        attempts: BoundedList(attempts),
        claim: super::allocation::projected_claim(read, w)?,
        merged_awaiting_acceptance: false,
        acceptance_report: brief.acceptance_report.clone(),
        detail_cursor,
        detail_coverage: Coverage {
            state: if truncated {
                CoverageState::Partial
            } else {
                CoverageState::Complete
            },
            gaps: Vec::new(),
            more_gaps: false,
        },
        comment_count: counter(brief.comments.len() as i64),
        handoff_count: counter(brief.handoffs.len() as i64),
        pending_plan,
        dependent_ids: BoundedList(dependent_ids),
        manual_order: Counter::new(w.manual_order.max(0)).ok(),
        github_issue: None,
        readiness: extras.readiness,
        pending_readiness: extras.pending_readiness,
        acceptance_tests: acceptance_tests.map(|t| BoundedList(t.into_iter().take(128).collect())),
        queue_revision: Some(rev(queue_revision)),
        workflow_revision: Some(rev(workflow_revision)),
    })
}
