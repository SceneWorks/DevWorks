//! Feature execution-readiness assessments (AGENT-12, AGENT-18..20; AD79).
//!
//! An assessment is a feature-owned record bound to the plan it reviewed (the active approved
//! plan, or the pending draft before adoption: its slot revision and digest) and to the digest
//! of each scope unit of that plan ([`super::model::plan_units`]: the feature-wide unit and one
//! per live requirement). It holds every finding with its disposition:
//!
//! * a **resolved decision** (never blocking) records the decision, its confidence and evidence;
//! * a **bounded investigation** records its objective, authority/resource limits, closure
//!   condition and the dependent work it gates (only that work, and only implementation);
//! * a **needs-user decision** records the specific question and the recommended answer.
//!
//! A blocking finding names the requirement it threatens (none: the feature-wide outcome), what
//! is missing or contradictory, the concrete consequence, what was checked or attempted, why
//! autonomous resolution is insufficient and the exact answer or evidence that closes it.
//!
//! Freshness is computed, unit by unit, against the active plan: a unit whose digest changed (or
//! a new requirement) is stale; every unchanged unit keeps its evidence, so a material change
//! invalidates only the affected scope. Readiness is derived — a current assessment of the
//! active plan with no open blocking finding — and is reported apart from plan approval; no
//! caller asserts it, and approving a plan never closes a finding. Re-assessing an unchanged plan
//! cannot add a blocker or reopen a settled finding without new evidence; settled findings an
//! assessment leaves out are kept. A blocking finding closes with a closure note and, for anyone
//! but the user, evidence; a human may accept a finding's identified risk through the approvals
//! item's decision seam ([`accept_finding_risk`]), which never waives a missing prerequisite.
use super::model::{
    FEATURE_UNIT, active_of, dropped, pending_of, plan_digest as digest_of_draft, plan_units,
};
use super::{R, check_artifact, conflict, invalid, not_found, refuse, stale, visible_feature};
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::IdKind;
use crate::ports::storage::{
    FEATURE, NewEvent, StorageError, StoredReadiness, StoredWork, WorkRead, WriteTransaction,
};
use crate::protocol::provider::Digest;
use crate::protocol::records::{
    FindingState, ReadinessAssessment, ReadinessChange, ReadinessDisposition, ReadinessFinding,
    ReadinessFreshness,
};
use crate::protocol::support::BoundedList;
use crate::protocol::{ActorId, DomainErrorCode, EventId, Id, RevisionDimension, RootId};
use crate::services::runs::scope::{PlanState, Readiness, ScopeUnit};
use crate::services::sessions::{content, counter, rev};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// History aggregate of a feature's readiness (`aggregate_id` = feature ID).
pub const READINESS_AGGREGATE: &str = "feature_readiness";

/// The stored body of an assessment.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Assessment {
    pub id: Id,
    pub plan_revision: i64,
    pub plan_digest: String,
    pub reviewed_pending: bool,
    /// Digest of each reviewed scope unit (`""`: the feature-wide unit).
    pub units: BTreeMap<String, String>,
    pub summary: String,
    pub findings: Vec<ReadinessFinding>,
    pub assessed_by: ActorId,
    pub assessed_at_ms: i64,
}
impl Assessment {
    /// The assessment of the feature's active plan in a stored readiness record: the only one
    /// that gates work.
    pub fn of(v: &Value) -> Option<Self> {
        Slots::of(v).active
    }
}

/// A feature's stored readiness: the assessment of its active plan and, apart from it, the
/// assessment of its pending replacement draft. The pending one gates nothing and never changes
/// the active one; adopting exactly the draft it reviewed promotes it. Every finding open in the
/// active one is open in the pending one too ([`carry_open`]), so adoption never clears it. One
/// revision covers both.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Slots {
    #[serde(default)]
    pub active: Option<Assessment>,
    #[serde(default)]
    pub pending: Option<Assessment>,
}
impl Slots {
    pub fn of(v: &Value) -> Self {
        serde_json::from_value(v.clone()).unwrap_or_default()
    }
    fn value(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }
}

fn open_blocking(f: &ReadinessFinding) -> bool {
    f.blocking && matches!(f.state, FindingState::Open)
}

/// The scope units of the active plan of `feature`, or of its pending draft before approval.
fn reference_units(
    read: &dyn WorkRead,
    feature: &Id,
) -> Result<Option<BTreeMap<String, String>>, StorageError> {
    let Some(slot) = read.feature_plan(feature)? else {
        return Ok(None);
    };
    if let Some(a) = active_of(slot.approved.as_ref()) {
        return Ok(Some(plan_units(&a.draft)));
    }
    Ok(pending_of(slot.pending.as_ref()).map(|p| plan_units(&p.draft)))
}
/// The units whose evidence is stale: changed digests and units the assessment never reviewed.
fn stale_units(
    reviewed: &BTreeMap<String, String>,
    current: &BTreeMap<String, String>,
) -> Vec<String> {
    current
        .iter()
        .filter(|(k, d)| reviewed.get(*k) != Some(*d))
        .map(|(k, _)| k.clone())
        .collect()
}

/// The wire assessment of `feature`'s active plan (with freshness against it), if one exists:
/// the readiness that gates its work.
pub fn assessment(
    read: &dyn WorkRead,
    feature: &StoredWork,
) -> Result<Option<ReadinessAssessment>, StorageError> {
    if feature.kind != FEATURE {
        return Ok(None);
    }
    let Some(stored) = read.readiness(&feature.id)? else {
        return Ok(None);
    };
    let Some(a) = Slots::of(&stored.body).active else {
        return Ok(None);
    };
    let active = read
        .feature_plan(&feature.id)?
        .and_then(|s| active_of(s.approved.as_ref()));
    let current = active
        .as_ref()
        .map(|a| plan_units(&a.draft))
        .unwrap_or_default();
    project(feature, &a, stored.revision, &current, active.is_some()).map(Some)
}

/// The wire assessment of `feature`'s pending replacement draft (with freshness against that
/// draft), if one exists. It is never ready: it gates nothing until the draft is adopted.
pub fn pending_assessment(
    read: &dyn WorkRead,
    feature: &StoredWork,
) -> Result<Option<ReadinessAssessment>, StorageError> {
    if feature.kind != FEATURE {
        return Ok(None);
    }
    let Some(stored) = read.readiness(&feature.id)? else {
        return Ok(None);
    };
    let Some(a) = Slots::of(&stored.body).pending else {
        return Ok(None);
    };
    let current = read
        .feature_plan(&feature.id)?
        .and_then(|s| pending_of(s.pending.as_ref()))
        .map(|p| plan_units(&p.draft))
        .unwrap_or_default();
    project(feature, &a, stored.revision, &current, false).map(Some)
}

/// The wire form of one assessment, fresh against `current` units; ready only when it can gate
/// (`gates`), is fresh and holds no open blocking finding.
fn project(
    feature: &StoredWork,
    a: &Assessment,
    revision: i64,
    current: &BTreeMap<String, String>,
    gates: bool,
) -> Result<ReadinessAssessment, StorageError> {
    let stale: Vec<Id> = if current.is_empty() {
        vec![feature.id.clone()]
    } else {
        stale_units(&a.units, current)
            .into_iter()
            .map(|k| {
                if k == FEATURE_UNIT {
                    feature.id.clone()
                } else {
                    Id::new(k).unwrap_or_else(|_| feature.id.clone())
                }
            })
            .collect()
    };
    let fresh = stale.is_empty();
    let ready = gates && fresh && !a.findings.iter().any(open_blocking);
    Ok(ReadinessAssessment {
        id: a.id.clone(),
        revision: rev(revision),
        plan_revision: rev(a.plan_revision),
        plan_digest: Digest::try_from(a.plan_digest.clone())
            .map_err(|_| StorageError::InvalidRecord("stored plan digest".into()))?,
        reviewed_pending: a.reviewed_pending,
        freshness: if fresh {
            ReadinessFreshness::Current
        } else {
            ReadinessFreshness::Stale
        },
        stale_scope_keys: BoundedList(stale.into_iter().take(128).collect()),
        ready,
        summary: content(&a.summary),
        findings: BoundedList(a.findings.iter().take(128).cloned().collect()),
        assessed_by: a.assessed_by.clone(),
        assessed_at_ms: counter(a.assessed_at_ms),
    })
}

/// The execution authority run control evaluates for a feature (its scope port): the active
/// approved plan's units and, for each, readiness from the stored assessment. `None` when the
/// stored plan is not one this service wrote (fixtures): the caller keeps its plain reading.
pub fn scope_plan(
    approved: &Value,
    pending_revision: Option<i64>,
    readiness: Option<&(i64, Value)>,
) -> Option<PlanState> {
    let active = active_of(Some(approved))?;
    let units = plan_units(&active.draft);
    let assessment = readiness.and_then(|(r, v)| Assessment::of(v).map(|a| (*r, a)));
    let unit = |key: &str, digest: &String| -> ScopeUnit {
        let readiness = match &assessment {
            None => Readiness::Missing,
            Some((r, a)) => {
                // A finding that names the work it gates holds only that work (enforced at
                // dispatch by [`gating_finding`], AGENT-19); only a finding of a whole scope
                // (or of the feature-wide outcome) holds a scope unit.
                let blocking = a.findings.iter().find(|f| {
                    open_blocking(f)
                        && f.affected_work_ids.0.is_empty()
                        && (f.scope_key.is_none()
                            || f.scope_key.as_ref().is_some_and(|k| k.as_str() == key))
                });
                if a.units.get(key) != Some(digest) {
                    Readiness::Stale {
                        assessment_revision: *r,
                    }
                } else if let Some(f) = blocking {
                    Readiness::Blocked {
                        finding: f.id.as_str().to_owned(),
                    }
                } else {
                    Readiness::Current {
                        assessment_revision: *r,
                    }
                }
            }
        };
        ScopeUnit {
            digest: digest.clone(),
            readiness,
        }
    };
    let feature = unit(FEATURE_UNIT, units.get(FEATURE_UNIT)?);
    let named = units
        .iter()
        .filter(|(k, _)| k.as_str() != FEATURE_UNIT)
        .map(|(k, d)| (k.clone(), unit(k, d)))
        .collect();
    Some(PlanState::Approved {
        revision: active.plan_revision,
        pending_revision,
        feature,
        units: named,
    })
}

/// The open blocking finding that gates `work` (a bounded investigation or a decision whose
/// affected work names it): that work's implementation waits for its closure; unrelated work of
/// the feature keeps moving.
pub fn gating_finding(readiness: &Value, work: &Id) -> Option<String> {
    let a = Assessment::of(readiness)?;
    a.findings
        .iter()
        .find(|f| open_blocking(f) && f.affected_work_ids.0.contains(work))
        .map(|f| f.id.as_str().to_owned())
}

fn nonblank(c: &Option<crate::protocol::support::BoundedText<0, 65536>>) -> bool {
    c.as_ref().is_some_and(|t| !t.0.trim().is_empty())
}
fn blank(t: &str) -> bool {
    t.trim().is_empty()
}

/// Checks one finding of an assessment against the reviewed plan and the feature's work.
fn check_finding(
    f: &ReadinessFinding,
    requirements: &BTreeSet<String>,
    work: &BTreeSet<String>,
) -> R<()> {
    let bad = |kind: &'static str| Err(invalid(kind, Some(&f.id)));
    for r in &f.requirement_ids.0 {
        if !requirements.contains(r.as_str()) {
            return Err(not_found("requirement", r).into());
        }
    }
    if let Some(k) = &f.scope_key
        && !requirements.contains(k.as_str())
    {
        return Err(not_found("requirement", k).into());
    }
    for w in &f.affected_work_ids.0 {
        if !work.contains(w.as_str()) {
            return Err(not_found("work", w).into());
        }
    }
    if matches!(f.state, FindingState::RiskAccepted) {
        // Only a human decision accepts a finding's risk.
        return bad("finding_state");
    }
    match f.disposition {
        ReadinessDisposition::ResolvedDecision => {
            if f.blocking
                || !matches!(f.state, FindingState::Resolved)
                || !nonblank(&f.decision)
                || f.confidence.is_none()
            {
                return bad("resolved_decision");
            }
        }
        ReadinessDisposition::BoundedInvestigation => {
            if !nonblank(&f.objective)
                || !nonblank(&f.limits)
                || blank(&f.closure_condition.0)
                || f.affected_work_ids.0.is_empty()
            {
                return bad("investigation");
            }
        }
        ReadinessDisposition::NeedsUserDecision => {
            if !nonblank(&f.question) || !nonblank(&f.recommendation) {
                return bad("user_decision");
            }
        }
    }
    if f.blocking {
        // A blocker must justify itself: its requirement (unless the whole outcome is
        // undefined), what is missing, the consequence, the attempts made, why autonomous
        // resolution is insufficient and what closes it.
        let feature_wide = f.scope_key.is_none() && f.requirement_ids.0.is_empty();
        if (!feature_wide && f.requirement_ids.0.is_empty())
            || blank(&f.missing.0)
            || blank(&f.consequence.0)
            || f.attempts.0.is_empty()
            || blank(&f.insufficiency.0)
            || blank(&f.closure_condition.0)
        {
            return bad("blocking_finding");
        }
    }
    if f.blocking
        && matches!(f.state, FindingState::Resolved)
        && !nonblank(&f.closure_note)
        && f.closure_evidence.0.is_empty()
    {
        return bad("closure");
    }
    Ok(())
}

/// Whether two statements of a finding are the same (a repeated, unchanged finding).
fn same_finding(a: &ReadinessFinding, b: &ReadinessFinding) -> bool {
    serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
}

/// A re-assessment cannot settle or weaken a finding that is still open: it stays open (closing
/// is `CloseFinding`, with its note and evidence, or a human's risk acceptance), stays blocking
/// and a prerequisite when it was, and keeps gating at least the scope and work it gated. An
/// empty `affectedWorkIds` gates the whole scope, so naming work narrows it (refused); widening
/// (more work, or no work named) is allowed.
fn keeps_gate(stored: &ReadinessFinding, given: &ReadinessFinding) -> R<()> {
    if !matches!(stored.state, FindingState::Open) {
        return Ok(());
    }
    let weakened = !matches!(given.state, FindingState::Open)
        || (stored.blocking && !given.blocking)
        || (stored.prerequisite && !given.prerequisite)
        || (stored.scope_key.is_some()
            && given.scope_key.is_some()
            && stored.scope_key != given.scope_key)
        || (stored.scope_key.is_none() && given.scope_key.is_some())
        || (stored.affected_work_ids.0.is_empty() && !given.affected_work_ids.0.is_empty())
        || !stored
            .affected_work_ids
            .0
            .iter()
            .all(|w| given.affected_work_ids.0.contains(w));
    if weakened {
        return Err(conflict("open_finding_needs_closure", &stored.id));
    }
    Ok(())
}

/// Whether `given` gates more than `stored` did: an open blocker where it was not one (it was
/// non-blocking or settled), a new prerequisite, a wider scope (every scope where it named
/// one) or more work (the whole scope where it named work, or work it did not name).
fn strengthens(stored: &ReadinessFinding, given: &ReadinessFinding) -> bool {
    if !open_blocking(given) {
        return false;
    }
    let named = &stored.affected_work_ids.0;
    !open_blocking(stored)
        || (given.prerequisite && !stored.prerequisite)
        || (stored.scope_key.is_some() && given.scope_key.is_none())
        || (!named.is_empty()
            && (given.affected_work_ids.0.is_empty()
                || given.affected_work_ids.0.iter().any(|w| !named.contains(w))))
}

/// Reconciles the pending generation with the active one: every finding still open in `active`
/// stays open in `pending`, gating at least what it gates there (AGENT-20: an open finding
/// leaves only through `CloseFinding` or a human's finding-specific risk acceptance, both of
/// which act on every generation at once, so neither a draft's assessment nor plan approval
/// clears it). A pending statement of the finding is kept while it is open and keeps the active
/// gate; otherwise the active statement replaces it. Returns whether `pending` changed.
fn carry_open(active: &Assessment, pending: &mut Assessment) -> bool {
    let mut changed = false;
    for o in active
        .findings
        .iter()
        .filter(|f| matches!(f.state, FindingState::Open))
    {
        match pending.findings.iter_mut().find(|p| p.id == o.id) {
            None => {
                pending.findings.push(o.clone());
                changed = true;
            }
            Some(p) => {
                if !matches!(p.state, FindingState::Open) || keeps_gate(o, p).is_err() {
                    *p = o.clone();
                    changed = true;
                }
            }
        }
    }
    changed
}

fn evidence_keys(f: &ReadinessFinding) -> BTreeSet<String> {
    f.evidence
        .0
        .iter()
        .chain(&f.closure_evidence.0)
        .map(|a| format!("{}:{}", a.artifact_id.as_str(), a.version_id.as_str()))
        .collect()
}

fn record(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    feature: &StoredWork,
    revision: i64,
    kind: &str,
    payload: Value,
) -> Result<(), StorageError> {
    let mut payload = payload;
    if let Some(m) = payload.as_object_mut() {
        m.insert(
            "principalKind".into(),
            json!(format!("{:?}", ctx.principal_kind())),
        );
    }
    let id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id,
        actor: ctx.principal().clone(),
        project_id: Some(feature.project_id.clone()),
        root_id: Some(RootId(feature.id.clone())),
        operation_id: None,
        aggregate_kind: READINESS_AGGREGATE.into(),
        aggregate_id: feature.id.as_str().to_owned(),
        aggregate_revision: revision,
        kind: kind.into(),
        payload,
    })?;
    Ok(())
}

/// Applies a `features.edit` readiness change to `feature` (inside the edit's unit of work).
pub(crate) fn apply_change(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    feature: &StoredWork,
    change: &ReadinessChange,
) -> R<()> {
    let stored = tx.work().readiness(&feature.id)?;
    let slots = stored
        .as_ref()
        .map(|s| Slots::of(&s.body))
        .unwrap_or_default();
    let current_rev = stored.as_ref().map(|s| s.revision);
    match change {
        ReadinessChange::Assess {
            expected_assessment_revision,
            plan_revision,
            plan_digest,
            summary,
            findings,
        } => {
            let expected = expected_assessment_revision.as_ref().map(|r| r.get());
            if expected != current_rev {
                return Err(stale(
                    RevisionDimension::Plan,
                    "readiness",
                    &feature.id,
                    expected.unwrap_or(1),
                    current_rev.unwrap_or(1),
                ));
            }
            // The assessment binds the plan it reviewed: the active plan or the pending draft,
            // at exactly this revision and digest.
            let slot = tx.work().feature_plan(&feature.id)?;
            let active = slot.as_ref().and_then(|s| active_of(s.approved.as_ref()));
            let pending = slot
                .as_ref()
                .and_then(|s| pending_of(s.pending.as_ref()).zip(s.pending_revision));
            let digest = String::from(plan_digest.clone());
            let (draft, reviewed_pending) = match (&active, &pending) {
                (Some(a), _) if a.plan_revision == plan_revision.get() && a.digest == digest => {
                    (a.draft.clone(), false)
                }
                (_, Some((p, r))) if *r == plan_revision.get() && p.digest == digest => {
                    (p.draft.clone(), true)
                }
                _ => return Err(conflict("plan_changed", &feature.id)),
            };
            debug_assert_eq!(digest_of_draft(&draft), digest);
            // Each generation has its own assessment. A pending draft's first one starts from
            // the active plan's: an open finding stays open in the replacement until it closes
            // (one the active plan's assessment opens later is carried into it then).
            let previous = if reviewed_pending {
                slots.pending.clone().or_else(|| slots.active.clone())
            } else {
                slots.active.clone()
            };
            let requirements: BTreeSet<String> = draft
                .requirements
                .0
                .iter()
                .filter(|r| !super::model::dropped(r))
                .map(|r| r.id.as_str().to_owned())
                .collect();
            let work = feature_work(tx.work(), feature)?;
            let ids: Vec<Id> = findings.0.iter().map(|f| f.id.clone()).collect();
            super::id_set(&ids, "finding_ids")?;
            for f in &findings.0 {
                let stored = previous
                    .as_ref()
                    .and_then(|p| p.findings.iter().find(|o| o.id == f.id));
                // A settled finding repeated unchanged is kept as it is (a human's risk
                // acceptance included); anything else is validated as a new statement.
                if stored.is_some_and(|o| same_finding(o, f)) {
                    continue;
                }
                check_finding(f, &requirements, &work)?;
                if let Some(o) = stored {
                    keeps_gate(o, f)?;
                }
                for a in f.evidence.0.iter().chain(&f.closure_evidence.0) {
                    check_artifact(tx, ctx, &feature.project_id, a)?;
                }
            }
            let units = plan_units(&draft);
            let mut out: Vec<ReadinessFinding> = findings.0.clone();
            if let Some(prev) = &previous {
                // Closure is checked without reopening settled choices: at an unchanged plan, a
                // new blocker, a reopened finding or a strengthened one needs newly discovered
                // evidence (AGENT-20).
                let same_plan = prev.plan_digest == digest;
                for f in &findings.0 {
                    let old = prev.findings.iter().find(|o| o.id == f.id);
                    let new_evidence = match old {
                        Some(o) => !evidence_keys(f).is_subset(&evidence_keys(o)),
                        None => !f.evidence.0.is_empty(),
                    };
                    let reopened = old.is_some_and(|o| {
                        !matches!(o.state, FindingState::Open)
                            && matches!(f.state, FindingState::Open)
                    });
                    let added_blocker = old.is_none() && open_blocking(f);
                    let strengthened = old.is_some_and(|o| strengthens(o, f));
                    if same_plan && (reopened || added_blocker || strengthened) && !new_evidence {
                        return Err(conflict("settled_without_new_evidence", &f.id));
                    }
                    if old.is_some_and(|o| matches!(o.state, FindingState::RiskAccepted))
                        && !matches!(f.state, FindingState::RiskAccepted)
                        && !new_evidence
                    {
                        return Err(conflict("settled_without_new_evidence", &f.id));
                    }
                }
                // Every finding the new assessment leaves out is kept: a settled one with its
                // closure, and an open one with its gate (an open finding leaves only through a
                // validated closure or a human's finding-specific risk acceptance).
                for o in &prev.findings {
                    if !out.iter().any(|f| f.id == o.id) {
                        if out.len() >= 128 {
                            return Err(invalid("finding_limit", Some(&o.id)));
                        }
                        out.push(o.clone());
                    }
                }
            }
            let body = Assessment {
                id: previous
                    .as_ref()
                    .map(|p| p.id.clone())
                    .unwrap_or_else(|| tx.new_id(IdKind::Link)),
                plan_revision: plan_revision.get(),
                plan_digest: digest.clone(),
                reviewed_pending,
                units,
                summary: summary.0.clone(),
                findings: out,
                assessed_by: ctx.principal().clone(),
                assessed_at_ms: tx.now_ms(),
            };
            let revision = current_rev.map_or(1, |r| r + 1);
            let mut next = slots.clone();
            if reviewed_pending {
                next.pending = Some(body.clone());
            } else {
                next.active = Some(body.clone());
                // A finding the active plan's assessment opens or widens also holds the pending
                // replacement, so adopting it cannot drop the finding.
                if let Some(p) = next.pending.as_mut()
                    && carry_open(&body, p)
                    && p.findings.len() > 128
                {
                    return Err(invalid("finding_limit", Some(&feature.id)));
                }
            }
            tx.work().put_readiness(
                &StoredReadiness {
                    feature_id: feature.id.clone(),
                    revision,
                    body: next.value(),
                },
                current_rev,
            )?;
            let open: Vec<&str> = body
                .findings
                .iter()
                .filter(|f| open_blocking(f))
                .map(|f| f.id.as_str())
                .collect();
            record(
                tx,
                ctx,
                feature,
                revision,
                "ReadinessAssessed",
                json!({
                    "planRevision": plan_revision.get(),
                    "reviewedPending": reviewed_pending,
                    "findings": body.findings.len(),
                    "openBlocking": open,
                }),
            )?;
        }
        ReadinessChange::CloseFinding {
            expected_assessment_revision,
            finding_id,
            note,
            evidence,
        } => {
            let Some(stored) = stored else {
                return Err(not_found("readiness", &feature.id).into());
            };
            if stored.revision != expected_assessment_revision.get() {
                return Err(stale(
                    RevisionDimension::Plan,
                    "readiness",
                    &feature.id,
                    expected_assessment_revision.get(),
                    stored.revision,
                ));
            }
            if blank(&note.0) {
                return Err(invalid("closure_note", Some(finding_id)));
            }
            for a in &evidence.0 {
                check_artifact(tx, ctx, &feature.project_id, a)?;
            }
            // The finding closes in every generation that holds it (an active plan's finding
            // carried into its pending replacement closes there too).
            let mut next = slots.clone();
            let mut found = false;
            for body in [next.active.as_mut(), next.pending.as_mut()]
                .into_iter()
                .flatten()
            {
                let Some(f) = body.findings.iter_mut().find(|f| &f.id == finding_id) else {
                    continue;
                };
                if !matches!(f.state, FindingState::Open) {
                    continue;
                }
                if f.blocking
                    && evidence.0.is_empty()
                    && ctx.principal_kind() != PrincipalKind::Human
                {
                    return Err(invalid("closure_evidence", Some(finding_id)));
                }
                f.state = FindingState::Resolved;
                f.closure_note = Some(note.clone());
                f.closure_evidence = evidence.clone();
                found = true;
            }
            if !found {
                let known = [slots.active.as_ref(), slots.pending.as_ref()]
                    .into_iter()
                    .flatten()
                    .any(|b| b.findings.iter().any(|f| &f.id == finding_id));
                return Err(if known {
                    conflict("finding_settled", finding_id)
                } else {
                    not_found("finding", finding_id).into()
                });
            }
            let revision = stored.revision + 1;
            tx.work().put_readiness(
                &StoredReadiness {
                    feature_id: feature.id.clone(),
                    revision,
                    body: next.value(),
                },
                Some(stored.revision),
            )?;
            record(
                tx,
                ctx,
                feature,
                revision,
                "ReadinessFindingClosed",
                json!({"findingId": finding_id.as_str(), "evidence": evidence.0.len()}),
            )?;
        }
    }
    Ok(())
}

/// The live requirements that `card` covered and no other card of the feature still covers
/// (`card` has just left the feature), per generation: of the active plan, and of the pending
/// draft. A generation's gap is its own: the other one may still cover the requirement.
fn uncovered_by(
    tx: &mut dyn WriteTransaction,
    feature: &StoredWork,
    card: &Id,
) -> R<(Vec<Id>, Vec<Id>)> {
    let Some(slot) = tx.work().feature_plan(&feature.id)? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let members = feature_work(tx.work(), feature)?;
    let lost = |d: Option<crate::protocol::records::PlanDraft>| -> Vec<Id> {
        d.map(|d| {
            d.requirements
                .0
                .iter()
                .filter(|r| !dropped(r))
                .filter(|r| {
                    let covering = &r.covered_by_task_ids.0;
                    covering.contains(card)
                        && !covering.iter().any(|t| members.contains(t.as_str()))
                })
                .map(|r| r.id.clone())
                .collect()
        })
        .unwrap_or_default()
    };
    Ok((
        lost(active_of(slot.approved.as_ref()).map(|a| a.draft)),
        lost(pending_of(slot.pending.as_ref()).map(|p| p.draft)),
    ))
}

/// The ID stem of the coverage findings of requirement `r` lost through `card`; each departure
/// is its own occurrence (`<stem>-<n>`).
fn coverage_stem(r: &Id, card: &Id) -> String {
    format!(
        "coverage-{}",
        &crate::fingerprint::sha256_hex(format!("{}|{}", r.as_str(), card.as_str()).as_bytes())
            [..32]
    )
}
fn of_stem(f: &ReadinessFinding, stem: &str) -> bool {
    f.id.as_str()
        .strip_prefix(stem)
        .is_some_and(|rest| rest.starts_with('-'))
}

/// A card left `feature` (unlinked, or moved to another feature). A requirement it was the
/// only covering card of has lost its delivery: that is surfaced as an open, blocking readiness
/// finding of that requirement alone (AGENT-19: a local gap blocks only the affected scope, and
/// AGENT-20: changed scope is what adds a blocker), never by silently dropping coverage.
///
/// The finding goes to the generation that lost the coverage: a gap of the active plan into the
/// active assessment and, as [`carry_open`] keeps every open active finding, into the pending
/// one too; a gap only the pending draft has (the active plan still covers the requirement)
/// into the pending assessment alone, so it never holds the active plan and a rejected
/// replacement takes it with it, while an adopted one keeps it. Findings flow from active to
/// pending, never back.
///
/// Each departure is its own occurrence with its own ID: a card that leaves again after an
/// earlier finding was closed (it was linked back) is changed scope again, so a new open
/// finding is recorded and the earlier one keeps its closure; while an occurrence is still open
/// the same departure adds none. It closes as any finding does (`CloseFinding`). Without a
/// stored assessment of a generation nothing of it is ready to begin with, and its first
/// assessment reviews the plan as it is. Returns the requirements either generation lost.
pub(crate) fn coverage_lost(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    feature: &StoredWork,
    card: &Id,
) -> R<Vec<Id>> {
    let (active_lost, pending_lost) = uncovered_by(tx, feature, card)?;
    let mut uncovered = active_lost.clone();
    for r in &pending_lost {
        if !uncovered.contains(r) {
            uncovered.push(r.clone());
        }
    }
    let Some(stored) = tx.work().readiness(&feature.id)? else {
        return Ok(uncovered);
    };
    if uncovered.is_empty() {
        return Ok(uncovered);
    }
    let mut next = Slots::of(&stored.body);
    let mut added = Vec::new();
    for r in &uncovered {
        let in_active = active_lost.contains(r);
        let in_pending = in_active || pending_lost.contains(r);
        let stem = coverage_stem(r, card);
        let bodies = [next.active.as_ref(), next.pending.as_ref()];
        let open = |b: Option<&Assessment>| {
            b.is_some_and(|b| {
                b.findings
                    .iter()
                    .any(|f| of_stem(f, &stem) && matches!(f.state, FindingState::Open))
            })
        };
        // This departure is already an open finding: nothing is added.
        if (in_active && open(bodies[0])) || (!in_active && open(bodies[1])) {
            continue;
        }
        let occurrence = bodies
            .iter()
            .flatten()
            .flat_map(|b| b.findings.iter())
            .filter(|f| of_stem(f, &stem))
            .map(|f| f.id.as_str().to_owned())
            .collect::<BTreeSet<_>>()
            .len()
            + 1;
        let id =
            Id::new(format!("{stem}-{occurrence}")).map_err(|_| invalid("finding_id", None))?;
        let finding: ReadinessFinding = serde_json::from_value(json!({
            "id": id.as_str(),
            "disposition": "NeedsUserDecision",
            "blocking": true,
            "prerequisite": false,
            "state": "Open",
            "requirementIds": [r.as_str()],
            "scopeKey": r.as_str(),
            "affectedWorkIds": [],
            "summary": format!("Requirement {} has no covering card: {} left the feature", r.as_str(), card.as_str()),
            "missing": format!("a card of the feature that delivers requirement {}", r.as_str()),
            "consequence": format!("requirement {} cannot be delivered or verified by the feature's work", r.as_str()),
            "attempts": [format!("checked the feature's remaining cards: none covers requirement {}", r.as_str())],
            "insufficiency": "which work delivers the requirement, or whether it is dropped, is a planning decision",
            "closureCondition": format!("a card of the feature covers requirement {} again, or it is dropped with a recorded decision", r.as_str()),
            "question": format!("Which card should cover requirement {}, or should it be dropped?", r.as_str()),
            "recommendation": format!("Link {} back or plan a card covering the requirement", card.as_str()),
            "evidence": [],
            "closureEvidence": [],
        }))
        .map_err(|_| invalid("finding", Some(&id)))?;
        // The generation that lost the coverage holds it once it reviewed the requirement; one
        // open in the active generation is open in the pending one too (as [`carry_open`]
        // keeps it).
        let mut recorded = false;
        for (body, lost) in [
            (next.active.as_mut(), in_active),
            (next.pending.as_mut(), in_pending),
        ] {
            let Some(body) = body else {
                continue;
            };
            let holds = lost && (body.units.contains_key(r.as_str()) || recorded);
            if holds && body.findings.len() < 128 {
                body.findings.push(finding.clone());
                recorded = true;
            }
        }
        if recorded {
            added.push(id.as_str().to_owned());
        }
    }
    if added.is_empty() {
        return Ok(uncovered);
    }
    let revision = stored.revision + 1;
    tx.work().put_readiness(
        &StoredReadiness {
            feature_id: feature.id.clone(),
            revision,
            body: next.value(),
        },
        Some(stored.revision),
    )?;
    record(
        tx,
        ctx,
        feature,
        revision,
        "ReadinessCoverageLost",
        json!({"workId": card.as_str(), "findingIds": added,
            "requirementIds": uncovered.iter().map(Id::as_str).collect::<Vec<_>>()}),
    )?;
    Ok(uncovered)
}

/// The cards of `feature` (IDs), for affected-work validation.
fn feature_work(read: &dyn WorkRead, feature: &StoredWork) -> R<BTreeSet<String>> {
    use crate::ports::storage::{WorkFilter, WorkPosition, WorkSelection};
    let filter = WorkFilter {
        project: feature.project_id.clone(),
        selection: WorkSelection::FeatureTasks(feature.id.clone()),
        status_ids: Vec::new(),
        repository_ids: Vec::new(),
        text: None,
    };
    let mut out = BTreeSet::new();
    let mut after: Option<WorkPosition> = None;
    loop {
        let page = read.works(&filter, after.as_ref(), 500)?;
        let more = page.len() == 500;
        after = page.last().map(WorkPosition::of);
        out.extend(page.iter().map(|w| w.id.as_str().to_owned()));
        if !more {
            return Ok(out);
        }
    }
}

/// The approvals item's seam for an explicit, finding-specific human acceptance of an
/// identified risk. Only a human decides; a finding recording a missing prerequisite (a
/// credential, dependency, contradictory acceptance or required verification) cannot be waived.
/// Plan approval never reaches this.
pub fn accept_finding_risk(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    feature_id: &Id,
    finding_id: &Id,
    expected_revision: i64,
    rationale: &str,
) -> R<ReadinessAssessment> {
    if ctx.principal_kind() != PrincipalKind::Human {
        return Err(refuse(
            DomainErrorCode::PermissionDenied,
            "risk_acceptance",
            Some(finding_id),
        ));
    }
    let feature = visible_feature(tx.work(), ctx, feature_id)?;
    super::open_project(tx, ctx, &feature.project_id)?;
    let Some(stored) = tx.work().readiness(&feature.id)? else {
        return Err(not_found("readiness", feature_id).into());
    };
    if stored.revision != expected_revision {
        return Err(stale(
            RevisionDimension::Plan,
            "readiness",
            feature_id,
            expected_revision,
            stored.revision,
        ));
    }
    if blank(rationale) {
        return Err(conflict("finding_settled", finding_id));
    }
    let mut next = Slots::of(&stored.body);
    let mut found = false;
    for body in [next.active.as_mut(), next.pending.as_mut()]
        .into_iter()
        .flatten()
    {
        let Some(f) = body.findings.iter_mut().find(|f| &f.id == finding_id) else {
            continue;
        };
        if f.prerequisite {
            return Err(conflict("prerequisite_not_waivable", finding_id));
        }
        if !matches!(f.state, FindingState::Open) {
            continue;
        }
        f.state = FindingState::RiskAccepted;
        f.closure_note = Some(content(rationale));
        found = true;
    }
    if !found {
        return Err(conflict("finding_settled", finding_id));
    }
    let revision = stored.revision + 1;
    tx.work().put_readiness(
        &StoredReadiness {
            feature_id: feature.id.clone(),
            revision,
            body: next.value(),
        },
        Some(stored.revision),
    )?;
    record(
        tx,
        ctx,
        &feature,
        revision,
        "ReadinessRiskAccepted",
        json!({"findingId": finding_id.as_str(), "rationale": rationale}),
    )?;
    let shown = match assessment(tx.work(), &feature)? {
        Some(a) => Some(a),
        None => pending_assessment(tx.work(), &feature)?,
    };
    Ok(shown.expect("just recorded"))
}

/// Records which scope units a plan adoption made stale (their evidence is no longer current;
/// every other unit keeps it).
pub(crate) fn record_adoption(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    feature: &StoredWork,
) -> Result<(), StorageError> {
    let Some(mut stored) = tx.work().readiness(&feature.id)? else {
        return Ok(());
    };
    // The pending draft's assessment becomes the active one when it reviewed exactly the plan
    // adopted; one of another draft is dropped (it was never authority). The active plan's
    // assessment is kept otherwise, its changed units now stale.
    let mut slots = Slots::of(&stored.body);
    let adopted = tx
        .work()
        .feature_plan(&feature.id)?
        .and_then(|s| active_of(s.approved.as_ref()));
    if let Some(pending) = slots.pending.take() {
        if adopted.as_ref().is_some_and(|a| {
            a.plan_revision == pending.plan_revision && a.digest == pending.plan_digest
        }) {
            let mut promoted = pending;
            promoted.reviewed_pending = false;
            // Approval never closes a finding (AGENT-20): one still open in the replaced
            // plan's assessment stays open in the adopted one.
            if let Some(old) = &slots.active {
                carry_open(old, &mut promoted);
            }
            slots.active = Some(promoted);
        }
        let next = StoredReadiness {
            feature_id: feature.id.clone(),
            revision: stored.revision + 1,
            body: slots.value(),
        };
        tx.work().put_readiness(&next, Some(stored.revision))?;
        stored = next;
    }
    let Some(a) = slots.active else {
        return Ok(());
    };
    let Some(current) = reference_units(tx.work(), &feature.id)? else {
        return Ok(());
    };
    let stale = stale_units(&a.units, &current);
    let kept: Vec<&String> = current.keys().filter(|k| !stale.contains(k)).collect();
    record(
        tx,
        ctx,
        feature,
        stored.revision,
        "ReadinessInvalidated",
        json!({"staleUnits": stale, "currentUnits": kept}),
    )
}
