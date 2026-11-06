use super::{model, service::*, *};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedCommand;
use crate::operations::approvals as ops;
use crate::ports::IdKind;
use crate::ports::storage::*;
use crate::protocol::records::{ApprovalContent, ProposalDecision};
use crate::protocol::results::OperationResult;
use crate::services::sessions::{Failure, not_found, project_visible};
use serde_json::{Value, json};

impl ApprovalService {
    pub(crate) fn propose(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        c: &AdmittedCommand,
        i: &ops::ProposeInput,
    ) -> Result<OperationResult, Failure> {
        let scope = scope(ctx, &i.scope)?;
        let payload = i.payload.as_ref().ok_or_else(unsupported)?;
        let result = match payload {
            ApprovalContent::Settings { entries } => {
                if i.operation_kind.0 != "settings.configure" || i.target.id != scope.target_id() {
                    return Err(invalid("settings_proposal"));
                }
                if model::hash(&serde_json::to_value(entries).unwrap()) != i.content_digest.0 {
                    return Err(invalid("content_digest"));
                }
                let p = crate::settings::service::stage_proposal(
                    tx,
                    ctx,
                    self.roles.as_ref(),
                    &scope,
                    i.target.revision.get(),
                    &entries.0,
                )
                .map_err(settings_failure)?;
                model::settings(&p, p.base_revision, true)?
            }
            ApprovalContent::FeaturePlan { proposal_id } => {
                let p = tx
                    .work()
                    .proposal(proposal_id)?
                    .ok_or_else(|| not_found("approval", proposal_id))?;
                let f = tx
                    .work()
                    .work(&p.feature_id)?
                    .filter(|w| project_visible(ctx, Some(&w.project_id)))
                    .ok_or_else(|| not_found("feature", &p.feature_id))?;
                if scope != ConfigScope::Project(f.project_id.clone())
                    || i.operation_kind.0 != crate::services::work::plans::PLAN_APPROVAL
                    || i.target.id != p.feature_id
                    || i.target.revision.get() != p.plan_revision
                    || i.content_digest.0 != p.plan_digest
                {
                    return Err(invalid("plan_proposal"));
                }
                model::plan(
                    &p,
                    &f.project_id,
                    tx.work().feature_plan(&f.id)?.as_ref(),
                    true,
                )?
            }
            ApprovalContent::SavedOperation {
                schema_revision,
                input,
            } => {
                let request = self.decoded(
                    &i.operation_kind.0,
                    schema_revision.0.parse().unwrap_or(0),
                    &input.0,
                )?;
                let prepared = c
                    .prepared::<PreparedOwner>()
                    .ok_or_else(|| invalid("owner_preview"))?;
                self.validate_owner(tx, ctx, &request, &prepared.preview)?;
                if request.fingerprint.as_str() != i.content_digest.0
                    || prepared.preview.scope != scope
                    || prepared.preview.target_id != i.target.id
                    || prepared.preview.target_revision != i.target.revision.get()
                {
                    return Err(invalid("reviewed_binding"));
                }
                let p = stage_saved(tx, ctx, i, &request, &prepared.preview)?;
                model::generic(&p, true, None)?
            }
            ApprovalContent::FindingRisk {
                feature_id,
                finding_id,
                assessment_revision,
            } => {
                if i.operation_kind.0 != "features.accept_finding_risk"
                    || i.target.id != *finding_id
                    || i.target.revision.get() != assessment_revision.get()
                {
                    return Err(invalid("finding_risk"));
                }
                let f = tx
                    .work()
                    .work(feature_id)?
                    .filter(|w| project_visible(ctx, Some(&w.project_id)))
                    .ok_or_else(|| not_found("feature", feature_id))?;
                if scope != ConfigScope::Project(f.project_id) {
                    return Err(invalid("finding_scope"));
                }
                let r = tx
                    .work()
                    .readiness(feature_id)?
                    .ok_or_else(|| not_found("readiness", feature_id))?;
                if r.revision != assessment_revision.get() {
                    return Err(conflict(feature_id));
                }
                let finding = find_finding(&r.body, finding_id)
                    .ok_or_else(|| not_found("finding", finding_id))?;
                if finding.get("prerequisite") == Some(&json!(true)) {
                    return Err(invalid("prerequisite_not_waivable"));
                }
                if model::hash(&serde_json::to_value(payload).unwrap()) != i.content_digest.0 {
                    return Err(invalid("content_digest"));
                }
                let binding = json!({"featureId":feature_id,"finding":finding,"assessmentRevision":r.revision,"assessmentDigest":model::hash(&r.body),"restoreEpoch":ctx.restore_epoch()});
                let p = stage_generic(
                    tx,
                    ctx,
                    i,
                    scope,
                    json!({"content":payload,"reviewed":binding,"binding":binding,"invocation":super::replay::save_context(ctx)}),
                )?;
                model::generic(&p, true, None)?
            }
        };
        Ok(OperationResult::ApprovalsPropose { value: result })
    }
    pub(crate) fn record(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        c: &AdmittedCommand,
        i: &ops::RecordHumanDecisionInput,
    ) -> Result<OperationResult, Failure> {
        if !ctx.human_decision_matches(i)
            || tx.actors().actor_kind(ctx.principal())? != Some(PrincipalKind::Human)
        {
            return Err(denied("trusted_human_decision"));
        }
        if i.rationale.0.trim().is_empty() {
            return Err(invalid("rationale"));
        }
        let approve = matches!(i.decision, ProposalDecision::Approve);
        let id = &i.proposal_id;
        let expected = i.expected_revision.get();
        let (source, before, mut after) = if let Some(p) = tx.configuration().proposal(id)? {
            if p.namespace != "settings" || !project_visible(ctx, p.scope.project_id()) {
                return Err(not_found("approval", id).into());
            }
            let current = tx
                .configuration()
                .current(&p.scope, "settings")?
                .map_or(1, |c| c.revision);
            let before = model::settings(&p, current, true)?;
            check_review(&before, i)?;
            let changed = if approve {
                crate::settings::service::approve_proposal(tx, ctx, id, expected)
                    .map_err(settings_failure)?
            } else {
                crate::settings::service::reject_proposal(tx, ctx, id, expected)
                    .map_err(settings_failure)?
            };
            let after = model::settings(&changed, current, true)?;
            (ApprovalSource::Settings, before, after)
        } else if let Some(p) = tx.work().proposal(id)? {
            let f = tx
                .work()
                .work(&p.feature_id)?
                .filter(|w| project_visible(ctx, Some(&w.project_id)))
                .ok_or_else(|| not_found("approval", id))?;
            let before = model::plan(
                &p,
                &f.project_id,
                tx.work().feature_plan(&f.id)?.as_ref(),
                true,
            )?;
            check_review(&before, i)?;
            if approve && p.reviewed_snapshot.is_none() {
                return Err(invalid("legacy_snapshot_unavailable"));
            }
            crate::services::work::plans::decide_plan_approval(tx, ctx, id, expected, approve)?;
            let changed = tx
                .work()
                .proposal(id)?
                .ok_or_else(|| not_found("approval", id))?;
            let after = model::plan(
                &changed,
                &f.project_id,
                tx.work().feature_plan(&f.id)?.as_ref(),
                true,
            )?;
            (ApprovalSource::FeaturePlan, before, after)
        } else {
            let p = tx
                .decisions()
                .approval(id)?
                .filter(|p| project_visible(ctx, p.scope.project_id()))
                .ok_or_else(|| not_found("approval", id))?;
            let before = model::generic(&p, true, None)?;
            check_review(&before, i)?;
            if p.restore_epoch != tx.store().restore_epoch {
                return Err(conflict(id));
            }
            if approve {
                match p.payload["content"]["kind"].as_str() {
                    Some("FindingRisk") => {
                        let payload: ApprovalContent =
                            serde_json::from_value(p.payload["content"].clone())
                                .map_err(|_| invalid("finding_risk"))?;
                        let ApprovalContent::FindingRisk {
                            feature_id,
                            finding_id,
                            assessment_revision,
                        } = payload
                        else {
                            unreachable!()
                        };
                        let r = tx
                            .work()
                            .readiness(&feature_id)?
                            .ok_or_else(|| not_found("readiness", &feature_id))?;
                        if model::hash(&r.body) != p.payload["binding"]["assessmentDigest"] {
                            return Err(conflict(id));
                        }
                        crate::services::work::readiness::accept_finding_risk(
                            tx,
                            ctx,
                            &feature_id,
                            &finding_id,
                            assessment_revision.get(),
                            &i.rationale.0,
                        )?;
                    }
                    Some("SavedOperation") => {
                        let original = super::replay::restore_context_tx(tx, &p, None)?;
                        let content: ApprovalContent =
                            serde_json::from_value(p.payload["content"].clone())
                                .map_err(|_| invalid("saved_operation"))?;
                        let ApprovalContent::SavedOperation {
                            schema_revision,
                            input,
                        } = content
                        else {
                            unreachable!()
                        };
                        if p.payload["authority"] != authority_binding(tx, &original)? {
                            return Err(conflict(id));
                        }
                        let request = self.decoded(
                            &p.operation_kind,
                            schema_revision.0.parse().unwrap_or(0),
                            &input.0,
                        )?;
                        let prepared = c
                            .prepared::<PreparedOwner>()
                            .ok_or_else(|| invalid("owner_preview"))?;
                        if serde_json::to_value(&prepared.preview.binding).unwrap()
                            != p.payload["binding"]
                        {
                            return Err(conflict(id));
                        }
                        self.validate_owner(tx, &original, &request, &prepared.preview)?;
                    }
                    _ => return Err(unsupported()),
                }
            }
            if p.payload["content"]["kind"] == "SavedOperation" {
                let content: ApprovalContent = serde_json::from_value(p.payload["content"].clone())
                    .map_err(|_| invalid("saved_operation"))?;
                let ApprovalContent::SavedOperation {
                    schema_revision,
                    input,
                } = content
                else {
                    unreachable!()
                };
                let request = self.decoded(
                    &p.operation_kind,
                    schema_revision.0.parse().unwrap_or(0),
                    &input.0,
                )?;
                let stored = OwnerPreview {
                    scope: p.scope.clone(),
                    target_id: p.target_id.clone(),
                    target_revision: p.target_revision,
                    summary: p.summary.clone(),
                    reviewed: p.payload["reviewed"].clone(),
                    binding: p.payload["binding"].clone(),
                };
                self.owners
                    .decide_source(tx, ctx, &request, &stored, approve)?;
            }
            let mut changed = p.clone();
            changed.revision += 1;
            changed.state = if approve { "Approved" } else { "Rejected" }.into();
            changed.decided_by = Some(ctx.principal().clone());
            changed.decided_at_ms = Some(tx.now_ms());
            changed.rationale = Some(i.rationale.0.clone());
            tx.decisions().put_approval(&changed, Some(p.revision))?;
            let after = model::generic(&changed, true, None)?;
            (ApprovalSource::Generic, before, after)
        };
        let at_ms = tx.now_ms();
        tx.decisions().insert_human_decision(&StoredHumanDecision {
            source,
            proposal_id: id.clone(),
            reviewed_proposal_revision: expected,
            decision: if approve { "Approved" } else { "Rejected" }.into(),
            rationale: Some(i.rationale.0.clone()),
            actor: ctx.principal().clone(),
            at_ms,
            content_digest: i.content_digest.0.clone(),
            scope_digest: before
                .scope_digest
                .as_ref()
                .map_or_else(String::new, |v| v.0.clone()),
            reviewed_snapshot: serde_json::to_value(&before).unwrap(),
        })?;
        if approve && source == ApprovalSource::Generic {
            let p = tx
                .decisions()
                .approval(id)?
                .ok_or_else(|| not_found("approval", id))?;
            if p.payload["content"]["kind"] == "SavedOperation" {
                self.create_resume(tx, ctx, &p)?;
            }
        }
        after.rationale = Some(i.rationale.clone());
        Ok(OperationResult::ApprovalsRecordHumanDecision { value: after })
    }
}
fn check_review(
    v: &crate::protocol::records::ApprovalViewRecord,
    i: &ops::RecordHumanDecisionInput,
) -> Result<(), Failure> {
    if v.revision != i.expected_revision
        || v.content_digest != i.content_digest
        || !matches!(v.state, crate::protocol::records::ApprovalState::Pending)
    {
        return Err(conflict(&i.proposal_id));
    }
    Ok(())
}
pub fn stage_saved(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    i: &ops::ProposeInput,
    request: &crate::admission::DecodedRequest,
    preview: &OwnerPreview,
) -> Result<StoredApproval, Failure> {
    if request.fingerprint.as_str() != i.content_digest.0
        || preview.target_id != i.target.id
        || preview.target_revision != i.target.revision.get()
        || scope(ctx, &i.scope)? != preview.scope
    {
        return Err(invalid("reviewed_binding"));
    }
    let authority = authority_binding(tx, ctx)?;
    let mut input = i.clone();
    input.summary = crate::services::sessions::content(&preview.summary);
    input.artifact = preview
        .reviewed
        .get("artifact")
        .map(|v| serde_json::from_value(v.clone()).map_err(|_| invalid("owner_artifact")))
        .transpose()?;
    if let Some(a) = &input.artifact {
        a.validate().map_err(|_| invalid("owner_artifact"))?;
    }

    stage_generic(
        tx,
        ctx,
        &input,
        preview.scope.clone(),
        json!({"content":i.payload,"reviewed":preview.reviewed,"binding":preview.binding,"authority":authority,"invocation":super::replay::save_context(ctx),"operationPreview":{"effect":format!("{:?}",request.effect),"targetIds":request.targets,"projectIds":request.project_targets,"requestDigest":request.fingerprint.as_str()}}),
    )
}
fn stage_generic(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    i: &ops::ProposeInput,
    scope: ConfigScope,
    payload: Value,
) -> Result<StoredApproval, Failure> {
    let p = StoredApproval {
        id: tx.new_id(IdKind::Event),
        scope,
        revision: 1,
        state: "Pending".into(),
        operation_kind: i.operation_kind.0.clone(),
        target_id: i.target.id.clone(),
        target_revision: i.target.revision.get(),
        content_digest: i.content_digest.0.clone(),
        scope_digest: model::hash(
            &json!({"owner":payload["binding"],"authority":payload.get("authority")}),
        ),
        summary: payload["reviewed"]
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or(&i.summary.0)
            .into(),
        artifact: i
            .artifact
            .as_ref()
            .map(|a| serde_json::to_value(a).unwrap()),
        payload,
        author: ctx.principal().clone(),
        author_kind: ctx.principal_kind(),
        created_at_ms: tx.now_ms(),
        decided_by: None,
        decided_at_ms: None,
        rationale: None,
        restore_epoch: tx.store().restore_epoch,
    };
    tx.decisions().put_approval(&p, None)?;
    crate::services::notifications::notify_approval(
        tx,
        ApprovalSource::Generic,
        &p.id,
        p.scope.project_id(),
        "Approval required",
        &p.summary,
    )?;
    Ok(p)
}
fn find_finding(v: &Value, id: &Id) -> Option<Value> {
    match v {
        Value::Object(m) => {
            if m.get("id").and_then(Value::as_str) == Some(id.as_str()) {
                Some(v.clone())
            } else {
                m.values().find_map(|v| find_finding(v, id))
            }
        }
        Value::Array(a) => a.iter().find_map(|v| find_finding(v, id)),
        _ => None,
    }
}

fn settings_failure(f: crate::settings::service::Failure) -> Failure {
    match f {
        crate::settings::service::Failure::Storage(e) => Failure::Storage(e),
        crate::settings::service::Failure::Refused(r) => {
            Failure::Refused(crate::services::sessions::Refusal {
                code: r.code,
                retry: r.retry,
                target_kind: r.target_kind,
                target_id: r.target_id,
            })
        }
    }
}

pub(crate) fn authority_binding(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
) -> Result<Value, StorageError> {
    let policy =
        crate::authority::policy::resolve_policy(tx.configuration(), ctx.project_id())?.digest();
    let principal=tx.authority().principal(ctx.principal())?.map(|p|json!({"revision":p.revision,"active":p.active,"kind":format!("{:?}",p.kind),"role":p.role.map(|r|r.as_str()),"projectId":p.project_id,"tools":p.tool_families}));
    Ok(
        json!({"policyDigest":policy,"principal":principal,"projectId":ctx.project_id(),"rootId":ctx.root_id(),"assignmentId":ctx.assignment_id(),"restoreEpoch":ctx.restore_epoch()}),
    )
}
