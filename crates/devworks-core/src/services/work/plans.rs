//! Feature creation, the feature-owned plan slot, plan-approval requests and their decision
//! seam, atomic plans and re-planning by reference.
use super::items::{
    NewCard, bump, check_evidence, completion_for, feature_scope, open_feature, tx_record,
};
use super::model::{ActivePlan, PendingPlan, WorkBrief, active_of, pending_of};
use super::workflow::{self, CLOSED, finished, semantic_of};
use super::{
    PLAN_ITEMS, PLAN_LINKS, R, WorkService, check_artifact, check_revision, conflict, event,
    id_set, invalid, member_repositories, not_found, open_project, refuse, require_title, stale,
    status_of, visible_feature,
};
use crate::context::{InvocationContext, PrincipalKind};
use crate::operations::{features as fops, tasks as tops};
use crate::ports::IdKind;
use crate::ports::storage::{
    FEATURE, PROJECT_QUEUE, StoredFeaturePlan, StoredProposal, StoredWork, WorkFilter,
    WorkPosition, WorkSelection, WriteTransaction, feature_queue,
};
use crate::protocol::provider::{ArtifactRef, Digest};
use crate::protocol::records::{
    AcceptanceCriterion, ApprovalState, AtomicWorkPlan, FeatureCreation, FeaturePlanDraft,
    PlanDraft, ProposalRecord, TaskPlanEntry, WorkRecord,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, Check, Page};
use crate::protocol::{DomainErrorCode, Id, ProjectId, RevisionDimension, RootId};
use crate::services::sessions::{content, label, rev};
use serde_json::json;
use std::collections::BTreeMap;

/// Reads the bytes of a staged atomic plan (an immutable artifact version, verified against its
/// digest by the reader).
pub trait StagedPlans: Send + Sync {
    fn read_plan(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
        max: usize,
    ) -> Result<Vec<u8>, String>;
}
impl StagedPlans for crate::services::artifacts::ArtifactService {
    fn read_plan(
        &self,
        digest: &str,
        generation: &str,
        byte_count: u64,
        max: usize,
    ) -> Result<Vec<u8>, String> {
        self.files
            .read_range(digest, generation, byte_count, 0, max)
            .map_err(|e| format!("{e:?}"))
    }
}

/// The operation kind a plan-approval request names.
pub const PLAN_APPROVAL: &str = "features.plan_approval";

/// The plan slot of `feature`, materializing an empty one (revision 1) for a feature recorded
/// before its first plan change.
fn slot(tx: &mut dyn WriteTransaction, feature: &Id) -> R<StoredFeaturePlan> {
    if let Some(s) = tx.work().feature_plan(feature)? {
        return Ok(s);
    }
    let s = StoredFeaturePlan {
        feature_id: feature.clone(),
        revision: 1,
        approved: None,
        approved_by: None,
        approved_at_ms: None,
        pending: None,
        pending_revision: None,
    };
    tx.work().put_feature_plan(&s, None)?;
    Ok(s)
}

/// Validates a plan draft for `feature` of `project`: its content and drop decisions resolve,
/// its repositories are project members, its requirement and criterion IDs are unique, the
/// tasks it says cover a requirement are the feature's, and every live requirement of the active
/// plan is still present (dropping one needs an explicit `Dropped` decision).
fn check_draft(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    project: &ProjectId,
    feature: Option<&StoredWork>,
    d: &PlanDraft,
) -> R<()> {
    check_artifact(tx, ctx, project, &d.content)?;
    member_repositories(tx, project, &d.repository_ids.0)?;
    let req_ids: Vec<Id> = d.requirements.0.iter().map(|r| r.id.clone()).collect();
    id_set(&req_ids, "requirements")?;
    let acc: Vec<Id> = d.acceptance.0.iter().map(|c| c.id.clone()).collect();
    id_set(&acc, "acceptance")?;
    let tests: Vec<Id> = d.acceptance_tests.0.iter().map(|c| c.id.clone()).collect();
    id_set(&tests, "acceptance_tests")?;
    for r in &d.requirements.0 {
        if let Some(a) = &r.drop_decision {
            check_artifact(tx, ctx, project, a)?;
        }
        for t in &r.covered_by_task_ids.0 {
            let ok = feature.is_some_and(|f| {
                tx.work()
                    .work(t)
                    .ok()
                    .flatten()
                    .is_some_and(|w| w.parent_feature_id.as_ref() == Some(&f.id))
            });
            if !ok {
                return Err(not_found("work", t).into());
            }
        }
    }
    let evidence: Vec<Id> = d
        .acceptance
        .0
        .iter()
        .chain(&d.acceptance_tests.0)
        .flat_map(|c| c.evidence_ids.0.iter().cloned())
        .collect();
    check_evidence(tx, project, &evidence)?;
    if let Some(f) = feature
        && let Some(active) = tx
            .work()
            .feature_plan(&f.id)?
            .and_then(|s| active_of(s.approved.as_ref()))
    {
        // Every requirement of the active plan stays in a replacement: a live one until it is
        // dropped with a decision, a dropped one with its recorded decision.
        for r in &active.draft.requirements.0 {
            if !d.requirements.0.iter().any(|x| x.id == r.id) {
                return Err(conflict("requirement_missing", &r.id));
            }
        }
    }
    Ok(())
}

fn proposal_record(p: &StoredProposal, artifact: Option<ArtifactRef>) -> R<ProposalRecord> {
    Ok(ProposalRecord {
        id: p.id.clone(),
        revision: rev(p.revision),
        content_digest: Digest::try_from(p.plan_digest.clone())
            .map_err(|_| invalid("plan_digest", None))?,
        state: match p.state.as_str() {
            "Approved" => ApprovalState::Approved,
            "Rejected" => ApprovalState::Rejected,
            "Superseded" => ApprovalState::Superseded,
            "Expired" => ApprovalState::Expired,
            _ => ApprovalState::Pending,
        },
        operation_kind: label(PLAN_APPROVAL, "plan"),
        target_id: p.feature_id.clone(),
        target_revision: rev(p.plan_revision),
        summary: content(&p.summary),
        artifact,
    })
}

/// `given` with the recorded state of each criterion kept from the draft it revises (the
/// pending draft, else the active plan): a criterion keeping its ID and text keeps its state and
/// evidence (retained re-plan behavior), so a revision never silently resets met criteria.
fn keep_plan_state(tx: &mut dyn WriteTransaction, feature: &Id, given: &PlanDraft) -> R<PlanDraft> {
    let slot = tx.work().feature_plan(feature)?;
    let base = slot.as_ref().and_then(|s| {
        pending_of(s.pending.as_ref())
            .map(|p| p.draft)
            .or_else(|| active_of(s.approved.as_ref()).map(|a| a.draft))
    });
    let Some(base) = base else {
        return Ok(given.clone());
    };
    let keep = |list: &[AcceptanceCriterion], from: &[AcceptanceCriterion]| {
        list.iter()
            .map(|c| match from.iter().find(|b| b.id == c.id) {
                Some(b) if b.text.0 == c.text.0 => b.clone(),
                _ => c.clone(),
            })
            .collect::<Vec<_>>()
    };
    let mut out = given.clone();
    out.acceptance = BoundedList(keep(&given.acceptance.0, &base.acceptance.0));
    out.acceptance_tests = BoundedList(keep(&given.acceptance_tests.0, &base.acceptance_tests.0));
    Ok(out)
}

/// Marks the feature's pending request superseded (its draft changed or was replaced).
fn supersede_pending(tx: &mut dyn WriteTransaction, feature: &Id) -> R<Option<Id>> {
    if let Some(p) = tx.work().latest_proposal(feature)?
        && p.state == "Pending"
    {
        let mut after = p.clone();
        after.state = "Superseded".into();
        after.revision = p.revision + 1;
        tx.work().put_proposal(&after, Some(p.revision))?;
        return Ok(Some(p.id));
    }
    Ok(None)
}

/// Bumps a feature's work revision for a plan-slot change visible on its record.
fn touch(tx: &mut dyn WriteTransaction, ctx: &InvocationContext, f: &StoredWork) -> R<StoredWork> {
    let mut after = f.clone();
    after.revision = f.revision + 1;
    after.updated_at_ms = tx.now_ms();
    after.updated_by = Some(ctx.principal().clone());
    tx.work().update_work(&after, f.revision)?;
    Ok(after)
}

/// Saves `draft` as the feature's pending revision (the active plan is untouched).
fn save_pending(
    tx: &mut dyn WriteTransaction,
    f: &StoredWork,
    draft: &PlanDraft,
) -> R<(StoredFeaturePlan, Option<Id>)> {
    let before = slot(tx, &f.id)?;
    let digest = super::model::plan_digest(draft);
    if pending_of(before.pending.as_ref()).is_some_and(|p| p.digest == digest) {
        return Ok((before, None));
    }
    let mut after = before.clone();
    after.revision = before.revision + 1;
    after.pending = Some(
        serde_json::to_value(PendingPlan {
            draft: draft.clone(),
            digest,
        })
        .map_err(|e| invalid_record(&e.to_string()))?,
    );
    after.pending_revision = Some(after.revision);
    tx.work().put_feature_plan(&after, Some(before.revision))?;
    let superseded = supersede_pending(tx, &f.id)?;
    Ok((after, superseded))
}
fn invalid_record(m: &str) -> super::super::sessions::Failure {
    super::super::sessions::Failure::Storage(crate::ports::storage::StorageError::InvalidRecord(
        m.to_owned(),
    ))
}

/// What a plan entry resolved to.
struct Planned {
    key: Id,
    work: StoredWork,
    deps: Vec<Id>,
}

impl WorkService {
    /// Inserts a feature with its (empty) plan slot and, when given, its pending plan.
    #[allow(clippy::too_many_arguments)]
    fn insert_feature(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        project: &ProjectId,
        title: &str,
        description: &str,
        repos: &[Id],
        status: Option<&Id>,
        priority: i64,
        caller_ref: Option<Id>,
        plan: Option<&PlanDraft>,
    ) -> R<StoredWork> {
        require_title(title)?;
        if i32::try_from(priority).is_err() {
            return Err(invalid("priority", None));
        }
        let repos = member_repositories(tx, project, repos)?;
        let status = match status {
            Some(s) => status_of(tx, project, s)?.0,
            None => {
                let id = workflow::default_status_id(project, workflow::DEFAULT_KEY);
                status_of(tx, project, &id)?.0
            }
        };
        if let Some(d) = plan {
            check_draft(tx, ctx, project, None, d)?;
        }
        let now = tx.now_ms();
        let brief = WorkBrief {
            description: description.to_owned(),
            ..WorkBrief::default()
        };
        let w = StoredWork {
            id: tx.new_id(IdKind::Link),
            project_id: project.clone(),
            kind: FEATURE.into(),
            parent_feature_id: None,
            status_id: Some(status.id.clone()),
            title: title.to_owned(),
            priority,
            manual_order: tx.work().next_rank(project, PROJECT_QUEUE)?,
            revision: 1,
            scope_revision: 1,
            created_at_ms: now,
            updated_at_ms: now,
            created_by: Some(ctx.principal().clone()),
            updated_by: Some(ctx.principal().clone()),
            search_key: super::model::search_key(title, description),
            caller_ref,
            completion: None,
            brief: brief.value(),
        };
        tx.work().insert_work(&w)?;
        tx.work().set_work_repositories(&w.id, &repos)?;
        slot(tx, &w.id)?;
        if let Some(d) = plan {
            save_pending(tx, &w, d)?;
        }
        bump(tx, project, &[PROJECT_QUEUE.to_owned()])?;
        event(
            tx,
            ctx,
            &w,
            "WorkCreated",
            json!({
                "kind": "feature",
                "title": w.title,
                "statusId": status.id.as_str(),
                "priority": w.priority,
                "repositoryIds": repos.iter().map(Id::as_str).collect::<Vec<_>>(),
                "pendingPlan": plan.is_some(),
            }),
        )?;
        Ok(w)
    }

    pub(crate) fn create_features(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &fops::CreateInput,
    ) -> R<DomainDecision> {
        open_project(tx, ctx, &i.project_id)?;
        let created = match &i.creation {
            FeatureCreation::Single {
                title,
                description,
                repository_ids,
                status_id,
                priority,
                plan,
            } => vec![self.insert_feature(
                tx,
                ctx,
                &i.project_id,
                &title.0,
                &description.0,
                &repository_ids.0,
                Some(status_id),
                i64::from(*priority),
                None,
                plan.as_ref(),
            )?],
            FeatureCreation::AtomicPlan { plan } => {
                self.apply_atomic(tx, ctx, &i.project_id, plan, false)?
            }
            FeatureCreation::StagedPlan { artifact } => {
                let plan = self.staged_plan(tx, ctx, &i.project_id, artifact)?;
                self.apply_atomic(tx, ctx, &i.project_id, &plan, false)?
            }
        };
        // The result lists the features (the recorded result carries only features); their
        // cards are read through tasks.list. Each is read again now that every write of the unit
        // of work is done (a link write advances its target's revision), so the next edit can
        // name the revision it is given.
        let mut items = Vec::new();
        for w in created.iter().filter(|w| w.kind == FEATURE) {
            let current = tx
                .work()
                .work(&w.id)?
                .ok_or_else(|| not_found("work", &w.id))?;
            items.push(tx_record(tx, &current)?);
        }
        Ok(DomainDecision::Applied {
            result: OperationResult::FeaturesCreate { value: page(items) },
        })
    }

    /// Reads and validates a staged atomic plan (an artifact the invocation may see).
    fn staged_plan(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        project: &ProjectId,
        a: &ArtifactRef,
    ) -> R<AtomicWorkPlan> {
        check_artifact(tx, ctx, project, a)?;
        let Some(reader) = &self.staged else {
            return Err(refuse(
                DomainErrorCode::UnsupportedCapability,
                "staged_plan",
                Some(&a.artifact_id),
            ));
        };
        let Some(v) = tx.artifacts().version(&a.version_id)? else {
            return Err(not_found("artifact", &a.artifact_id).into());
        };
        if v.byte_count < 0 || v.byte_count as usize > super::MAX_STAGED_PLAN {
            return Err(invalid("staged_plan", Some(&a.artifact_id)));
        }
        let bytes = reader
            .read_plan(
                &v.blob_digest,
                &v.blob_generation,
                v.byte_count as u64,
                super::MAX_STAGED_PLAN,
            )
            .map_err(|_| not_found("artifact_file", &a.artifact_id))?;
        // The fixed AtomicWorkPlan schema: strict decoding, then its bounds and recorded rules.
        let plan: AtomicWorkPlan = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("staged_plan", Some(&a.artifact_id)))?;
        plan.check()
            .map_err(|_| invalid("staged_plan", Some(&a.artifact_id)))?;
        Ok(plan)
    }

    /// Applies an atomic project plan inside the caller's unit of work: features (new, or with
    /// `allow_existing` an existing feature at its expected revision whose plan becomes the
    /// pending draft), their cards and every link, all validated before the unit of work
    /// commits (any refusal writes nothing). Returns the features and cards it created or
    /// changed.
    pub(crate) fn apply_atomic(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        project: &ProjectId,
        plan: &AtomicWorkPlan,
        allow_existing: bool,
    ) -> R<Vec<StoredWork>> {
        let cards: usize = plan.features.0.iter().map(|f| f.tasks.0.len()).sum();
        if cards > PLAN_ITEMS || plan.features.0.len() > super::PLAN_FEATURES {
            return Err(invalid("plan_size", None));
        }
        let links: usize = plan.dependency_edges.0.len()
            + plan
                .features
                .0
                .iter()
                .flat_map(|f| &f.tasks.0)
                .map(|t| t.dependency_keys.0.len())
                .sum::<usize>();
        if links > PLAN_LINKS {
            return Err(invalid("plan_size", None));
        }
        let mut refs: Vec<Id> = vec![plan.caller_ref.clone()];
        for f in &plan.features.0 {
            refs.push(f.caller_ref.clone());
            refs.extend(f.tasks.0.iter().map(|t| t.client_key.clone()));
        }
        id_set(&refs, "caller_ref")?;
        let mut keys: BTreeMap<String, Id> = BTreeMap::new();
        let mut out: Vec<StoredWork> = Vec::new();
        let mut planned: Vec<Planned> = Vec::new();
        for fd in &plan.features.0 {
            let feature = self.plan_feature(tx, ctx, project, fd, allow_existing)?;
            keys.insert(fd.caller_ref.as_str().to_owned(), feature.id.clone());
            let cards = self.plan_cards(tx, ctx, &feature, &fd.tasks.0, false)?;
            for c in &cards {
                keys.insert(c.key.as_str().to_owned(), c.work.id.clone());
            }
            out.push(feature);
            planned.extend(cards);
        }
        // Card links from each entry's dependency keys, then the plan's explicit edges.
        let mut incoming: BTreeMap<String, Vec<Id>> = BTreeMap::new();
        for c in &planned {
            let mut deps = Vec::new();
            for k in &c.deps {
                deps.push(resolve(tx, project, &keys, k)?);
            }
            incoming.insert(c.work.id.as_str().to_owned(), deps);
        }
        for e in &plan.dependency_edges.0 {
            let p = resolve(tx, project, &keys, &e.predecessor)?;
            let s = resolve(tx, project, &keys, &e.successor)?;
            if !incoming.contains_key(s.as_str()) {
                let current = tx.work().prerequisites(&s)?;
                incoming.insert(s.as_str().to_owned(), current);
            }
            let entry = incoming.get_mut(s.as_str()).expect("inserted above");
            if !entry.contains(&p) {
                entry.push(p);
            }
        }
        self.write_links(tx, ctx, project, &incoming)?;
        out.extend(planned.into_iter().map(|p| p.work));
        Ok(out)
    }

    /// Writes the incoming links of each target through the one link writer
    /// ([`super::deps::set_links`]: kinds, project, self, duplicates and completed work) and
    /// then refuses any cycle they close, checked after every write of the unit of work so
    /// links within the plan are covered.
    fn write_links(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        project: &ProjectId,
        incoming: &BTreeMap<String, Vec<Id>>,
    ) -> R<()> {
        let mut targets = Vec::with_capacity(incoming.len());
        for (target, deps) in incoming {
            let target = Id::new(target.clone()).map_err(|_| invalid("work_id", None))?;
            super::deps::set_links(tx, ctx, project, &target, deps)?;
            targets.push(target);
        }
        super::deps::check_cycles(tx, &targets)
    }

    fn plan_feature(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        project: &ProjectId,
        fd: &FeaturePlanDraft,
        allow_existing: bool,
    ) -> R<StoredWork> {
        match (&fd.existing_feature_id, allow_existing) {
            (Some(_), false) => Err(invalid("existing_feature_id", None)),
            (Some(id), true) => {
                let f = visible_feature(tx.work(), ctx, &id.0)?;
                if &f.project_id != project {
                    return Err(not_found("feature", &id.0).into());
                }
                let Some(expected) = &fd.expected_revision else {
                    return Err(invalid("expected_revision", Some(&f.id)));
                };
                check_revision(&f, expected.get())?;
                open_feature(tx, &f)?;
                let draft = keep_plan_state(tx, &f.id, &fd.plan)?;
                check_draft(tx, ctx, project, Some(&f), &draft)?;
                // Saving a draft never touches the active plan or its approval.
                let (_, superseded) = save_pending(tx, &f, &draft)?;
                let f = touch(tx, ctx, &f)?;
                event(
                    tx,
                    ctx,
                    &f,
                    "PlanDraftSaved",
                    json!({"via": "atomic_plan", "supersededProposal": superseded.as_ref().map(Id::as_str)}),
                )?;
                Ok(f)
            }
            (None, _) => {
                if tx.work().by_caller_ref(project, &fd.caller_ref)?.is_some() {
                    return Err(conflict("caller_ref", &fd.caller_ref));
                }
                self.insert_feature(
                    tx,
                    ctx,
                    project,
                    &fd.title.0,
                    &fd.description.0,
                    &fd.repository_ids.0,
                    None,
                    0,
                    Some(fd.caller_ref.clone()),
                    Some(&fd.plan),
                )
            }
        }
    }

    /// Creates or updates the cards of `feature` from plan entries (matched by existing ID, then
    /// by caller reference among the feature's cards). With `close_omitted`, the feature's other
    /// unfinished cards are closed (retained re-plan behavior: never deleted).
    fn plan_cards(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        feature: &StoredWork,
        entries: &[TaskPlanEntry],
        close_omitted: bool,
    ) -> R<Vec<Planned>> {
        let project = feature.project_id.clone();
        let scope = feature_scope(tx.work(), feature)?;
        let mut out: Vec<Planned> = Vec::new();
        let mut matched: std::collections::HashSet<Id> = std::collections::HashSet::new();
        for e in entries {
            require_title(&e.title.0)?;
            let repos = member_repositories(tx, &project, &e.repository_ids.0)?;
            if let Some(allowed) = &scope
                && repos.iter().any(|r| !allowed.contains(r))
            {
                // Routine breakdown stays inside the active approved scope.
                return Err(conflict("plan_scope", &feature.id));
            }
            let crit: Vec<Id> = e.acceptance.0.iter().map(|c| c.id.clone()).collect();
            id_set(&crit, "acceptance")?;
            let existing = match &e.existing_task_id {
                Some(id) => Some(
                    tx.work()
                        .work(id)?
                        .filter(|w| w.project_id == project)
                        .ok_or_else(|| not_found("work", id))?,
                ),
                None => match tx.work().by_caller_ref(&project, &e.client_key)? {
                    Some(id) => tx.work().work(&id)?,
                    None => None,
                },
            };
            let work = match existing {
                Some(w) => {
                    if w.kind == FEATURE {
                        return Err(conflict("link_kind", &w.id));
                    }
                    if w.parent_feature_id.as_ref() != Some(&feature.id) {
                        // Membership changes only through tasks.set_feature.
                        return Err(conflict("feature_membership", &w.id));
                    }
                    // Every existing entry names the revision it was planned from: a card's own
                    // edits advance only its revision, so without it a re-plan would overwrite a
                    // newer title, description, repositories or criteria.
                    let Some(r) = &e.expected_revision else {
                        return Err(invalid("expected_revision", Some(&w.id)));
                    };
                    check_revision(&w, r.get())?;
                    if !matched.insert(w.id.clone()) {
                        return Err(invalid("duplicate_entry", Some(&w.id)));
                    }
                    self.replan_card(tx, ctx, &w, e, &repos)?
                }
                None => {
                    if e.existing_task_id.is_some() || e.expected_revision.is_some() {
                        return Err(invalid("expected_revision", Some(&e.client_key)));
                    }
                    let acceptance: Vec<AcceptanceCriterion> = e
                        .acceptance
                        .0
                        .iter()
                        .map(|c| AcceptanceCriterion {
                            evidence_ids: BoundedList(Vec::new()),
                            state: crate::protocol::records::CriterionState::Unmet,
                            ..c.clone()
                        })
                        .collect();
                    let w = self.insert_card(
                        tx,
                        ctx,
                        NewCard {
                            project: &project,
                            feature: Some(&feature.id),
                            title: &e.title.0,
                            description: &e.description.0,
                            repositories: &repos,
                            status: None,
                            priority: i64::from(e.priority.unwrap_or(0)),
                            caller_ref: Some(e.client_key.clone()),
                            acceptance,
                        },
                    )?;
                    matched.insert(w.id.clone());
                    w
                }
            };
            out.push(Planned {
                key: e.client_key.clone(),
                work,
                deps: e.dependency_keys.0.clone(),
            });
        }
        if close_omitted {
            let statuses = workflow::ensure_workflow(tx, &project)?;
            let closed = statuses.iter().find(|s| s.semantic == CLOSED).cloned();
            for w in feature_cards(tx, feature)? {
                if matched.contains(&w.id) {
                    continue;
                }
                let finished_now = w
                    .status_id
                    .as_ref()
                    .and_then(|s| semantic_of(&statuses, s))
                    .is_some_and(finished);
                if finished_now {
                    continue;
                }
                let Some(closed) = &closed else {
                    return Err(conflict("no_closed_status", &project.0));
                };
                let mut after = w.clone();
                after.status_id = Some(closed.id.clone());
                after.completion = None;
                after.revision = w.revision + 1;
                after.updated_at_ms = tx.now_ms();
                after.updated_by = Some(ctx.principal().clone());
                tx.work().update_work(&after, w.revision)?;
                let pivot = live_assignments(tx, &w)?;
                event(
                    tx,
                    ctx,
                    &after,
                    "ClosedByReplan",
                    json!({"statusId": closed.id.as_str(), "pivotAssignments": pivot}),
                )?;
            }
        }
        Ok(out)
    }

    /// Applies a re-plan entry to an existing card. A criterion keeping its ID and text keeps
    /// its state and evidence; an omitted priority is kept; a re-plan never completes a card.
    fn replan_card(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        w: &StoredWork,
        e: &TaskPlanEntry,
        repos: &[Id],
    ) -> R<StoredWork> {
        let mut brief = WorkBrief::of(w)?;
        let mut after = w.clone();
        let mut changed: Vec<&str> = Vec::new();
        if e.title.0 != w.title {
            after.title = e.title.0.clone();
            changed.push("title");
        }
        if e.description.0 != brief.description {
            brief.description = e.description.0.clone();
            changed.push("description");
        }
        if repos != tx.work().work_repositories(&w.id)?.as_slice() {
            tx.work().set_work_repositories(&w.id, repos)?;
            changed.push("repositoryIds");
        }
        let acceptance: Vec<AcceptanceCriterion> = e
            .acceptance
            .0
            .iter()
            .map(|c| match brief.acceptance.iter().find(|s| s.id == c.id) {
                Some(s) if s.text.0 == c.text.0 => s.clone(),
                _ => AcceptanceCriterion {
                    evidence_ids: BoundedList(Vec::new()),
                    state: crate::protocol::records::CriterionState::Unmet,
                    ..c.clone()
                },
            })
            .collect();
        if serde_json::to_value(&acceptance).ok() != serde_json::to_value(&brief.acceptance).ok() {
            brief.acceptance = acceptance;
            changed.push("acceptance");
        }
        let mut queues = Vec::new();
        if let Some(p) = e.priority
            && i64::from(p) != w.priority
        {
            after.priority = i64::from(p);
            changed.push("priority");
            queues.push(w.queue());
        }
        if changed.is_empty() {
            return Ok(w.clone());
        }
        let statuses = tx.work().statuses(&w.project_id)?;
        let semantic = w
            .status_id
            .as_ref()
            .and_then(|s| semantic_of(&statuses, s))
            .unwrap_or("")
            .to_owned();
        after.completion = completion_for(&semantic, &brief.acceptance);
        after.search_key = super::model::search_key(&after.title, &brief.description);
        after.brief = brief.value();
        after.revision = w.revision + 1;
        after.updated_at_ms = tx.now_ms();
        after.updated_by = Some(ctx.principal().clone());
        tx.work().update_work(&after, w.revision)?;
        if !queues.is_empty() {
            bump(tx, &w.project_id, &queues)?;
        }
        let pivot = live_assignments(tx, w)?;
        event(
            tx,
            ctx,
            &after,
            "ChangedByReplan",
            json!({"changed": changed, "pivotAssignments": pivot}),
        )?;
        Ok(after)
    }

    pub(crate) fn sync_plan(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &tops::SyncPlanInput,
    ) -> R<DomainDecision> {
        let feature = visible_feature(tx.work(), ctx, &i.feature_id.0)?;
        open_project(tx, ctx, &feature.project_id)?;
        open_feature(tx, &feature)?;
        let s = slot(tx, &feature.id)?;
        if s.revision != i.expected_plan_revision.get() {
            return Err(stale(
                RevisionDimension::Plan,
                "plan",
                &feature.id,
                i.expected_plan_revision.get(),
                s.revision,
            ));
        }
        let queue = feature_queue(&feature.id);
        let set_revision = tx.work().queue_revision(&feature.project_id, &queue)?;
        if set_revision != i.expected_task_set_revision.get() {
            return Err(stale(
                RevisionDimension::Queue,
                "task_set",
                &feature.id,
                i.expected_task_set_revision.get(),
                set_revision,
            ));
        }
        if i.tasks.0.len() > PLAN_ITEMS {
            return Err(invalid("plan_size", None));
        }
        let keys: Vec<Id> = i.tasks.0.iter().map(|t| t.client_key.clone()).collect();
        id_set(&keys, "client_key")?;
        let planned = self.plan_cards(tx, ctx, &feature, &i.tasks.0, true)?;
        let project = feature.project_id.clone();
        let mut refs: BTreeMap<String, Id> = BTreeMap::new();
        for p in &planned {
            refs.insert(p.key.as_str().to_owned(), p.work.id.clone());
        }
        let mut incoming: BTreeMap<String, Vec<Id>> = BTreeMap::new();
        for p in &planned {
            let mut deps = Vec::new();
            for k in &p.deps {
                deps.push(resolve(tx, &project, &refs, k)?);
            }
            incoming.insert(p.work.id.as_str().to_owned(), deps);
        }
        self.write_links(tx, ctx, &project, &incoming)?;
        let project_plan = match (&i.project_plan, &i.staged_project_plan) {
            (Some(p), None) => Some(p.clone()),
            (None, Some(a)) => Some(self.staged_plan(tx, ctx, &project, a)?),
            (None, None) => None,
            (Some(_), Some(_)) => return Err(invalid("project_plan", None)),
        };
        if let Some(p) = &project_plan {
            self.apply_atomic(tx, ctx, &project, p, true)?;
        }
        bump(tx, &project, &[queue])?;
        event(
            tx,
            ctx,
            &feature,
            "PlanSynced",
            json!({
                "entries": i.tasks.0.len(),
                "projectPlan": project_plan.is_some(),
            }),
        )?;
        // The result is the cards the plan names, created or updated, in plan order (at most
        // 200, so one complete page), each read again after every write of the unit of work.
        // Cards the re-plan closed are recorded in history (ClosedByReplan) and listed by
        // tasks.list; the feature's other cards are not part of this answer, so the page never
        // claims more without a cursor.
        let mut items = Vec::with_capacity(planned.len());
        for p in &planned {
            let current = tx
                .work()
                .work(&p.work.id)?
                .ok_or_else(|| not_found("work", &p.work.id))?;
            items.push(tx_record(tx, &current)?);
        }
        let value = page(items);
        Ok(DomainDecision::Applied {
            result: OperationResult::TasksSyncPlan { value },
        })
    }

    pub(crate) fn edit_plan(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &fops::EditPlanInput,
    ) -> R<DomainDecision> {
        let f = visible_feature(tx.work(), ctx, &i.feature_id.0)?;
        check_revision(&f, i.expected_revision.get())?;
        open_project(tx, ctx, &f.project_id)?;
        let draft = keep_plan_state(tx, &f.id, &i.plan)?;
        check_draft(tx, ctx, &f.project_id, Some(&f), &draft)?;
        let before = slot(tx, &f.id)?;
        let (after_slot, superseded) = save_pending(tx, &f, &draft)?;
        if after_slot.revision == before.revision {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::FeaturesEditPlan {
                    value: tx_record(tx, &f)?,
                },
            });
        }
        let after = touch(tx, ctx, &f)?;
        event(
            tx,
            ctx,
            &after,
            "PlanDraftSaved",
            json!({
                "planRevision": after_slot.revision,
                "supersededProposal": superseded.as_ref().map(Id::as_str),
                // The active plan, its approval and effective scope are unchanged.
                "activePlanUnchanged": true,
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::FeaturesEditPlan {
                value: tx_record(tx, &after)?,
            },
        })
    }

    pub(crate) fn request_plan_approval(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        i: &fops::RequestPlanApprovalInput,
    ) -> R<DomainDecision> {
        let f = visible_feature(tx.work(), ctx, &i.feature_id.0)?;
        check_revision(&f, i.expected_revision.get())?;
        open_project(tx, ctx, &f.project_id)?;
        let s = slot(tx, &f.id)?;
        let digest = String::from(i.plan_digest.clone());
        let Some(pending) = pending_of(s.pending.as_ref())
            .filter(|p| s.pending_revision == Some(i.plan_revision.get()) && p.digest == digest)
        else {
            // The request binds the exact current draft; a changed draft makes it stale.
            return Err(conflict("plan_changed", &f.id));
        };
        if let Some(p) = tx.work().latest_proposal(&f.id)?
            && p.state == "Pending"
            && p.plan_revision == i.plan_revision.get()
            && p.plan_digest == digest
        {
            return Ok(DomainDecision::NoChange {
                result: OperationResult::FeaturesRequestPlanApproval {
                    value: proposal_record(&p, Some(pending.draft.content.clone()))?,
                },
            });
        }
        supersede_pending(tx, &f.id)?;
        let p = StoredProposal {
            id: tx.new_id(IdKind::Link),
            feature_id: f.id.clone(),
            revision: 1,
            plan_revision: i.plan_revision.get(),
            plan_digest: digest,
            state: "Pending".into(),
            requested_by: ctx.principal().clone(),
            requested_at_ms: tx.now_ms(),
            decided_by: None,
            decided_at_ms: None,
            summary: format!("Approve the revised plan of {}", f.title),
        };
        tx.work().put_proposal(&p, None)?;
        let after = touch(tx, ctx, &f)?;
        event(
            tx,
            ctx,
            &after,
            "PlanApprovalRequested",
            json!({
                "proposalId": p.id.as_str(),
                "planRevision": p.plan_revision,
                // Requesting approval changes no active authority or eligible work.
                "activePlanUnchanged": true,
            }),
        )?;
        Ok(DomainDecision::Applied {
            result: OperationResult::FeaturesRequestPlanApproval {
                value: proposal_record(&p, Some(pending.draft.content))?,
            },
        })
    }
}

/// The approvals item's decision seam for a plan-approval request: an authenticated **human**
/// decision on the exact pending revision the request bound. Approval adopts that draft as the
/// active plan in this unit of work (effective requirements, acceptance, goals and repository
/// scope, history, readiness invalidation of only the changed units) and clears the pending
/// slot; a concurrent draft edit makes the request stale (`plan_changed`) instead of approving
/// unseen content. Rejection leaves the active plan, its approval and execution controls
/// unchanged. Neither continues paused work nor closes readiness findings.
pub fn decide_plan_approval(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    proposal_id: &Id,
    expected_proposal_revision: i64,
    approve: bool,
) -> R<WorkRecord> {
    if ctx.principal_kind() != PrincipalKind::Human {
        return Err(refuse(
            DomainErrorCode::PermissionDenied,
            "plan_approval",
            Some(proposal_id),
        ));
    }
    let Some(p) = tx.work().proposal(proposal_id)? else {
        return Err(not_found("proposal", proposal_id).into());
    };
    let f = visible_feature(tx.work(), ctx, &p.feature_id)?;
    open_project(tx, ctx, &f.project_id)?;
    if p.revision != expected_proposal_revision {
        return Err(stale(
            RevisionDimension::Review,
            "proposal",
            proposal_id,
            expected_proposal_revision,
            p.revision,
        ));
    }
    if p.state != "Pending" {
        return Err(conflict("proposal_decided", proposal_id));
    }
    let s = slot(tx, &f.id)?;
    let Some(pending) = pending_of(s.pending.as_ref())
        .filter(|d| s.pending_revision == Some(p.plan_revision) && d.digest == p.plan_digest)
    else {
        return Err(conflict("plan_changed", &f.id));
    };
    let now = tx.now_ms();
    let mut decided = p.clone();
    decided.revision = p.revision + 1;
    decided.state = if approve { "Approved" } else { "Rejected" }.into();
    decided.decided_by = Some(ctx.principal().clone());
    decided.decided_at_ms = Some(now);
    tx.work().put_proposal(&decided, Some(p.revision))?;
    let after = touch(tx, ctx, &f)?;
    if approve {
        let previous = active_of(s.approved.as_ref());
        let active = ActivePlan {
            draft: pending.draft.clone(),
            digest: pending.digest.clone(),
            plan_revision: p.plan_revision,
            proposal_id: p.id.clone(),
        };
        let mut next = s.clone();
        next.revision = s.revision + 1;
        next.approved =
            Some(serde_json::to_value(&active).map_err(|e| invalid_record(&e.to_string()))?);
        next.approved_by = Some(ctx.principal().clone());
        next.approved_at_ms = Some(now);
        next.pending = None;
        next.pending_revision = None;
        tx.work().put_feature_plan(&next, Some(s.revision))?;
        let repos = id_set(&pending.draft.repository_ids.0, "repository_ids")?;
        tx.work().set_work_repositories(&f.id, &repos)?;
        super::readiness::record_adoption(tx, ctx, &after)?;
        event(
            tx,
            ctx,
            &after,
            "PlanAdopted",
            json!({
                "proposalId": p.id.as_str(),
                "planRevision": p.plan_revision,
                "previousPlanRevision": previous.map(|a| a.plan_revision),
                // Approval never continues paused work.
                "continued": false,
            }),
        )?;
    } else {
        event(
            tx,
            ctx,
            &after,
            "PlanRejected",
            json!({"proposalId": p.id.as_str(), "activePlanUnchanged": true}),
        )?;
    }
    tx_record(tx, &after)
}

/// A complete result page (callers bound it: at most 20 features or 200 cards). It never claims
/// another page, because these results have no continuation.
fn page(items: Vec<WorkRecord>) -> Page<WorkRecord> {
    debug_assert!(items.len() <= 500);
    Page {
        items: BoundedList(items.into_iter().take(500).collect()),
        next_cursor: None,
        has_more: false,
    }
}

/// A plan reference: a key of this plan, or the ID of an item of the project.
fn resolve(
    tx: &mut dyn WriteTransaction,
    project: &ProjectId,
    keys: &BTreeMap<String, Id>,
    k: &Id,
) -> R<Id> {
    if let Some(id) = keys.get(k.as_str()) {
        return Ok(id.clone());
    }
    match tx.work().work(k)? {
        Some(w) if &w.project_id == project => Ok(w.id),
        _ => Err(not_found("plan_reference", k).into()),
    }
}

/// Every card of `feature` in queue order.
fn feature_cards(tx: &mut dyn WriteTransaction, feature: &StoredWork) -> R<Vec<StoredWork>> {
    let filter = WorkFilter {
        project: feature.project_id.clone(),
        selection: WorkSelection::FeatureTasks(feature.id.clone()),
        status_ids: Vec::new(),
        repository_ids: Vec::new(),
        text: None,
    };
    let mut out = Vec::new();
    let mut after: Option<WorkPosition> = None;
    loop {
        let page = tx.work().works(&filter, after.as_ref(), 500)?;
        let more = page.len() == 500;
        after = page.last().map(WorkPosition::of);
        out.extend(page);
        if !more {
            return Ok(out);
        }
    }
}

/// The live assignments working on `w` (a re-plan that edits or closes it notifies them).
fn live_assignments(tx: &mut dyn WriteTransaction, w: &StoredWork) -> R<Vec<String>> {
    let root = RootId(super::history_root(w));
    Ok(
        crate::services::runs::root_assignments(tx.runs(), &root, true)?
            .into_iter()
            .filter(|a| a.work_id.0 == w.id)
            .map(|a| a.id.0.as_str().to_owned())
            .collect(),
    )
}
