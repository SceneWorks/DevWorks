//! Dreaming proposals use the native Decisions repository and notification owner. Reviewing
//! them through Decisions or through Dreaming reaches the same evidence and Knowledge checks.
use super::{DreamingService, authorize, checked_instruction, invalid, rev};
use crate::context::{InvocationContext, PrincipalKind};
use crate::operations::dreaming as ops;
use crate::ports::storage::{
    ApprovalSource, ConfigScope, DreamRunRow, KnowledgeScope, StorageError, StoredApproval,
    StoredHumanDecision, WriteTransaction,
};
use crate::protocol::provider::{ArtifactRef, Digest};
use crate::protocol::records::{
    ApprovalState, DreamEvidenceKind, DreamEvidenceReference, DreamProposalChange,
    ProposalDecision, ProposalRecord, RevisionedRef,
};
use crate::protocol::results::OperationResult;
use crate::protocol::{DomainErrorCode, Id, Revision, RevisionDimension};
use crate::services::kernel::{Failure, Refusal, content, label, not_found};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub const PROPOSAL_OPERATION: &str = "dreaming.propose";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Proposal {
    run_id: Id,
    targets: Vec<RevisionedRef>,
    change: Option<DreamProposalChange>,
    evidence: Vec<DreamEvidenceReference>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    proofs: Vec<Value>,
    rationale: String,
    prevalence: String,
    artifact: ArtifactRef,
}

impl DreamingService {
    pub(super) fn propose(
        &self,
        _tx: &mut dyn WriteTransaction,
        _ctx: &InvocationContext,
        input: &ops::ProposeInput,
    ) -> Result<OperationResult, Failure> {
        // The gateway shape remains decodable. Only the worker holding the scoped sweep
        // lease can stage a proposal, using the receipt for its selected private tool call.
        Err(Refusal::new(
            DomainErrorCode::PermissionDenied,
            "dreaming_worker_lease",
            Some(input.run_id.clone()),
        )
        .into())
    }

    pub(super) fn review(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &ops::ReviewInput,
    ) -> Result<OperationResult, Failure> {
        if !ctx.human_dreaming_review_matches(input)
            || tx.actors().actor_kind(ctx.principal())? != Some(PrincipalKind::Human)
            || input.rationale.0.trim().is_empty()
        {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "dreaming_review",
                Some(input.proposal_id.clone()),
            )
            .into());
        }
        let stored = tx
            .decisions()
            .approval(&input.proposal_id)?
            .filter(|p| p.operation_kind == PROPOSAL_OPERATION)
            .ok_or_else(|| not_found("dreaming_proposal", &input.proposal_id))?;
        if stored.revision != input.expected_revision.get() {
            return Err(StorageError::conflict(
                RevisionDimension::Review,
                "dreaming_proposal",
                stored.id,
                input.expected_revision.clone(),
                rev(stored.revision),
            )
            .into());
        }
        let approve = matches!(input.decision, ProposalDecision::Approve);
        decide_effect(tx, ctx, &stored, approve)?;
        let at = tx.now_ms();
        let actor = ctx.principal().clone();
        let mut changed = stored.clone();
        changed.revision += 1;
        changed.state = if approve { "Approved" } else { "Rejected" }.into();
        changed.decided_by = Some(actor.clone());
        changed.decided_at_ms = Some(at);
        changed.rationale = Some(input.rationale.0.clone());
        tx.decisions()
            .put_approval(&changed, Some(stored.revision))?;
        tx.decisions().insert_human_decision(&StoredHumanDecision {
            source: ApprovalSource::Generic,
            proposal_id: stored.id.clone(),
            reviewed_proposal_revision: stored.revision,
            decision: changed.state.clone(),
            rationale: changed.rationale.clone(),
            actor,
            at_ms: at,
            content_digest: stored.content_digest,
            scope_digest: stored.scope_digest,
            reviewed_snapshot: stored.payload["reviewed"].clone(),
        })?;
        Ok(OperationResult::DreamingReview {
            value: render(&changed)?,
        })
    }
}

pub(super) fn stage_from_tool(
    tx: &mut dyn WriteTransaction,
    run: &DreamRunRow,
    args: &Value,
    artifact: ArtifactRef,
    refreshed_sources: &BTreeSet<String>,
) -> Result<Value, Failure> {
    use crate::protocol::records::DreamProposalChange::*;
    let target = |id: &str, version: &str| {
        let id = args[id]
            .as_str()
            .and_then(|id| Id::new(id).ok())
            .ok_or_else(|| invalid("dreaming_proposal_target"))?;
        let revision = args[version]
            .as_i64()
            .and_then(|rev| Revision::new(rev).ok())
            .ok_or_else(|| invalid("dreaming_proposal_revision"))?;
        Ok::<_, Failure>(RevisionedRef { id, revision })
    };
    let change = match args["action"].as_str() {
        Some("retract") => Retract {},
        Some("supersede") => Supersede {
            replacement: serde_json::from_value(args["replacement"].clone())
                .map_err(|_| invalid("dreaming_proposal_replacement"))?,
        },
        Some("merge") => Merge {
            other: target("other", "other_expected_version")?,
        },
        Some("skill_draft") => SkillDraft {},
        Some("hook_draft") => HookDraft {},
        Some("test_draft") => TestDraft {},
        _ => return Err(invalid("dreaming_proposal_action").into()),
    };
    let requested = args["evidence"]
        .as_array()
        .ok_or_else(|| invalid("dreaming_proposal_evidence"))?;
    let selection = super::proposal_evidence_selection(requested);
    if matches!(&change, Supersede { replacement } if replacement.supersedes.is_some()) {
        return Err(invalid("dreaming_proposal_replacement").into());
    }
    if matches!(&change, SkillDraft {} | HookDraft {} | TestDraft {}) {
        required_text(args, "draft")?;
    }
    let proofs =
        crate::services::knowledge::dream_actions::load_proofs(tx.dreaming()?, run, &selection)?;
    let primary = target("target", "expected_version")?;
    let proves = |target: &RevisionedRef| {
        proofs.iter().any(|proof| {
            proof["kind"] == "entry"
                && proof["id"] == json!(target.id)
                && proof["revision"].as_i64() == Some(target.revision.get())
        })
    };
    if !proves(&primary) || matches!(&change, Merge { other } if !proves(other)) {
        return Err(invalid("dreaming_proposal_target_evidence").into());
    }
    let rationale = required_text(args, "rationale")?;
    let prevalence = required_text(args, "prevalence")?;
    let evidence = proofs
        .iter()
        .map(|proof| {
            let reference = required_text(proof, "id")?;
            let fingerprint = Digest::try_from(required_text(proof, "fingerprint")?)
                .map_err(|_| invalid("dreaming_proposal_evidence_fingerprint"))?;
            let kind = match proof["kind"].as_str() {
                Some("entry") => DreamEvidenceKind::Knowledge,
                Some("lesson_event" | "challenge") => DreamEvidenceKind::Lesson,
                Some("graph_node") => DreamEvidenceKind::Graph,
                Some("review") => DreamEvidenceKind::Review,
                Some("item" | "attempt" | "handoff" | "thread" | "message") => {
                    DreamEvidenceKind::Work
                }
                _ => return Err(invalid("dreaming_proposal_evidence_kind").into()),
            };
            Ok::<_, Failure>(DreamEvidenceReference {
                kind,
                reference: crate::protocol::support::BoundedText(reference),
                fingerprint,
                summary: content("Read in this Dreaming run"),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let proposal = Proposal {
        run_id: run.id.clone(),
        targets: vec![primary],
        change: Some(change),
        evidence,
        proofs,
        rationale,
        prevalence,
        artifact,
    };
    let (row, suppressed) = stage(tx, run, proposal, refreshed_sources)?;
    Ok(
        json!({"proposal":render(&row)?,"proposalId":row.id,"changed":false,"suppressed":suppressed}),
    )
}

fn required_text(value: &Value, key: &str) -> Result<String, Failure> {
    value[key]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| invalid("dreaming_proposal_text").into())
}

fn stamp(value: &Value) -> Result<String, Failure> {
    crate::jcs::canonicalize(value)
        .map(|text| crate::fingerprint::sha256_hex(text.as_bytes()))
        .map_err(|_| invalid("dreaming_proposal_identity").into())
}

fn stage(
    tx: &mut dyn WriteTransaction,
    run: &DreamRunRow,
    proposal: Proposal,
    refreshed_sources: &BTreeSet<String>,
) -> Result<(StoredApproval, bool), Failure> {
    check_targets(tx, &run.scope, &proposal)?;
    valid_draft(tx, &run.scope, &proposal.artifact)?;
    let evidence_times = verify_evidence(tx, run, &proposal.proofs, Some(refreshed_sources))?;
    let reviewed = serde_json::to_value(&proposal).map_err(|_| invalid("dreaming_proposal"))?;
    let digest = stamp(&reviewed)?;
    let now = tx.now_ms();
    let target = proposal
        .targets
        .first()
        .ok_or_else(|| invalid("dreaming_proposal_targets"))?;
    let action = required_text(&reviewed["change"], "kind")?;
    if let Some(prior_id) = tx
        .dreaming()?
        .dream_prior_proposal(&run.scope, &target.id, &action)?
    {
        let prior = tx
            .decisions()
            .approval(&prior_id)?
            .ok_or(StorageError::ViewUnavailable)?;
        if prior.state == "Pending" {
            return Ok((prior, false));
        }
        let old: Proposal = serde_json::from_value(prior.payload["reviewed"].clone())
            .map_err(|_| invalid("dreaming_proposal_snapshot"))?;
        let decision_at = prior
            .decided_at_ms
            .ok_or_else(|| invalid("dreaming_decision_time"))?;
        if !has_fresh_evidence(&proposal.proofs, evidence_times, &old.proofs, decision_at) {
            return Ok((prior, true));
        }
    }
    let id = tx.new_id(crate::ports::IdKind::Proposal);
    let stored = StoredApproval {
        id,
        scope: ConfigScope::from(&run.scope),
        revision: 1,
        state: "Pending".into(),
        operation_kind: PROPOSAL_OPERATION.into(),
        target_id: target.id.clone(),
        target_revision: target.revision.get(),
        content_digest: digest,
        scope_digest: stamp(&json!({"scope":run.scope.key(),"run":run.id}))?,
        summary: if proposal.rationale.is_empty() {
            "Review Dreaming proposal".into()
        } else {
            proposal.rationale
        },
        artifact: Some(json!(proposal.artifact)),
        payload: json!({"content":{"kind":"DreamingProposal","runId":run.id},
            "reviewed":reviewed,"binding":{"scope":run.scope.key()}}),
        author: tx.store().host_actor.clone(),
        author_kind: PrincipalKind::Host,
        created_at_ms: now,
        decided_by: None,
        decided_at_ms: None,
        rationale: None,
        restore_epoch: tx.store().restore_epoch,
    };
    tx.decisions().put_approval(&stored, None)?;
    super::super::notifications::notify_approval(
        tx,
        ApprovalSource::Generic,
        &stored.id,
        stored.scope.project_id(),
        "Dreaming proposal",
        &stored.summary,
    )?;
    Ok((stored, false))
}

fn has_fresh_evidence(
    proofs: &[Value],
    observed_at: Vec<i64>,
    prior: &[Value],
    rejected_at: i64,
) -> bool {
    proofs.iter().zip(observed_at).any(|(proof, at)| {
        at > rejected_at
            && !prior.iter().any(|previous| {
                proof["kind"] == previous["kind"]
                    && proof["id"] == previous["id"]
                    && proof["fingerprint"] == previous["fingerprint"]
            })
    })
}

pub(crate) fn decide_effect(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    row: &StoredApproval,
    approve: bool,
) -> Result<(), Failure> {
    if row.operation_kind != PROPOSAL_OPERATION
        || row.state != "Pending"
        || row.author == *ctx.principal()
        || row.restore_epoch != tx.store().restore_epoch
        || tx.actors().actor_kind(ctx.principal())? != Some(PrincipalKind::Human)
    {
        return Err(StorageError::ScopeDenied.into());
    }
    let proposed: Proposal = serde_json::from_value(row.payload["reviewed"].clone())
        .map_err(|_| invalid("dreaming_proposal_snapshot"))?;
    if stamp(&row.payload["reviewed"])? != row.content_digest {
        return Err(invalid("dreaming_proposal_digest").into());
    }
    let run = tx
        .dreaming()?
        .dream_run(&proposed.run_id)?
        .ok_or_else(|| not_found("dreaming_run", &proposed.run_id))?;
    authorize(ctx, &run.scope)?;
    if ConfigScope::from(&run.scope) != row.scope {
        return Err(StorageError::ScopeDenied.into());
    }
    if approve {
        super::active(tx.projects(), &run.scope)?;
        check_targets(tx, &run.scope, &proposed)?;
        valid_draft(tx, &run.scope, &proposed.artifact)?;
        if let Some(change) = proposed.change {
            verify_evidence(tx, &run, &proposed.proofs, None)?;
            let target = proposed
                .targets
                .first()
                .ok_or_else(|| invalid("dreaming_proposal_targets"))?;
            crate::services::knowledge::review_dream_change(
                tx,
                ctx,
                &run.scope,
                target,
                change,
                &proposed.rationale,
            )?;
            crate::services::knowledge::queue_claim_vectors(tx, ctx, &run.scope)?;
        }
    }
    Ok(())
}

fn check_targets(
    tx: &mut dyn WriteTransaction,
    scope: &KnowledgeScope,
    proposal: &Proposal,
) -> Result<(), Failure> {
    let mut targets = proposal.targets.clone();
    if let Some(DreamProposalChange::Merge { other }) = &proposal.change {
        if targets.first().is_some_and(|target| target.id == other.id) {
            return Err(invalid("dreaming_merge_target").into());
        }
        targets.push(other.clone());
    }
    if targets.is_empty() {
        return Err(invalid("dreaming_proposal_targets").into());
    }
    for (position, target) in targets.into_iter().enumerate() {
        let claim = tx
            .knowledge()
            .knowledge_claim(&target.id)?
            .filter(|claim| &claim.scope == scope && claim.lifecycle == "Active")
            .ok_or_else(|| not_found("knowledge_claim", &target.id))?;
        if claim.revision != target.revision.get() {
            return Err(StorageError::conflict(
                RevisionDimension::Claim,
                "knowledge_claim",
                target.id,
                target.revision,
                rev(claim.revision),
            )
            .into());
        }
        match &proposal.change {
            Some(DreamProposalChange::Retract {} | DreamProposalChange::Supersede { .. })
                if position == 0
                    && claim.lesson.as_ref().is_some_and(|lesson| {
                        matches!(
                            lesson.source.as_str(),
                            "Prompt" | "Review" | "RelayedPrompt" | "RelayedReview"
                        )
                    }) =>
            {
                return Err(invalid("dreaming_proposal_prompt_lesson").into());
            }
            Some(DreamProposalChange::Merge { .. }) if claim.lesson.is_none() => {
                return Err(invalid("dreaming_proposal_merge_requires_lessons").into());
            }
            Some(
                DreamProposalChange::SkillDraft {}
                | DreamProposalChange::HookDraft {}
                | DreamProposalChange::TestDraft {},
            ) => {
                let history = tx
                    .knowledge()
                    .knowledge_history_at(&target.id, claim.revision)?
                    .ok_or(StorageError::ViewUnavailable)?;
                if history.snapshot["lesson"]["ruleState"] != "Effective"
                    || !tx
                        .dreaming()?
                        .dream_lesson_recurred_while_served(&target.id)?
                {
                    return Err(invalid("dreaming_proposal_rule_recurrence").into());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn valid_draft(
    tx: &mut dyn WriteTransaction,
    scope: &KnowledgeScope,
    input: &ArtifactRef,
) -> Result<(), Failure> {
    if input.media_type.starts_with("text/") {
        return checked_instruction(tx, scope, input);
    }
    let expected = scope.project().cloned().map_or(
        crate::ports::storage::ArtifactScope::Workspace,
        crate::ports::storage::ArtifactScope::Project,
    );
    let store = tx.artifacts();
    if input.byte_length.get() > 512 * 1024
        || input.media_type != "application/json"
        || !store.artifact(&input.artifact_id)?.is_some_and(|artifact| {
            artifact.scope == expected
                && artifact.state == crate::ports::storage::ArtifactState::Live
        })
        || !store.version(&input.version_id)?.is_some_and(|version| {
            version.live() && super::super::artifacts::version_ref(&version) == *input
        })
    {
        return Err(StorageError::ScopeDenied.into());
    }
    Ok(())
}

fn verify_evidence(
    tx: &mut dyn WriteTransaction,
    run: &DreamRunRow,
    proofs: &[Value],
    refreshed_sources: Option<&BTreeSet<String>>,
) -> Result<Vec<i64>, Failure> {
    use crate::services::knowledge::dream_actions::{
        graph_proof_observed_at, work_proof_observed_at,
    };
    if proofs.is_empty() {
        return Err(invalid("dreaming_proposal_evidence").into());
    }
    proofs
        .iter()
        .map(|proof| {
            let at = match proof["kind"].as_str() {
                Some("entry" | "lesson_event" | "challenge") => knowledge_proof_at(tx, run, proof)?,
                Some("graph_node") => {
                    let id = required_text(proof, "id")?;
                    let fingerprint = required_text(proof, "fingerprint")?;
                    if refreshed_sources
                        .is_some_and(|sources| !sources.contains(&format!("{id}:{fingerprint}")))
                    {
                        None
                    } else {
                        graph_proof_observed_at(tx, run, proof)?
                    }
                }
                Some("item" | "review" | "attempt" | "handoff" | "thread" | "message") => {
                    work_proof_observed_at(tx, run, proof)?
                }
                _ => None,
            };
            at.ok_or_else(|| invalid("dreaming_evidence_changed").into())
        })
        .collect()
}

fn knowledge_proof_at(
    tx: &mut dyn WriteTransaction,
    run: &DreamRunRow,
    proof: &Value,
) -> Result<Option<i64>, Failure> {
    let kind = required_text(proof, "kind")?;
    let reference = required_text(proof, "id")?;
    let fingerprint = required_text(proof, "fingerprint")?;
    let selected = if kind == "entry" {
        reference.clone()
    } else {
        required_text(proof, "claimId")?
    };
    let id = Id::new(selected).map_err(|_| invalid("dreaming_evidence_reference"))?;
    let k = tx.knowledge();
    let Some(current) = k.knowledge_claim(&id)?.filter(|claim| {
        claim.scope == run.scope
            && claim.lifecycle != "Deleted"
            && proof["revision"].as_i64() == Some(claim.revision)
    }) else {
        return Ok(None);
    };
    let snapshot = k
        .knowledge_history_at(&id, current.revision)?
        .ok_or(StorageError::ViewUnavailable)?;
    if kind == "entry" {
        return Ok((stamp(&snapshot.snapshot)? == fingerprint).then_some(snapshot.at_ms));
    }
    if kind == "lesson_event" {
        for event in k.knowledge_lesson_events(&id, 500)? {
            if crate::fingerprint::sha256_hex(event.event_key.as_bytes()) != reference {
                continue;
            }
            let record = json!({"id":reference,"kind":"lesson_event","source":event.source,
                "source_ref":event.source_ref,"actor":event.actor,"at_ms":event.at_ms,"write_id":event.write_id});
            return Ok((stamp(&record)? == fingerprint).then_some(event.at_ms));
        }
    }
    if kind == "challenge"
        && let Some(challenge) = snapshot.snapshot["lesson"]["challenges"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["id"] == reference))
    {
        return Ok((stamp(challenge)? == fingerprint).then_some(snapshot.at_ms));
    }
    Ok(None)
}

fn render(row: &StoredApproval) -> Result<ProposalRecord, Failure> {
    let state = match row.state.as_str() {
        "Pending" => ApprovalState::Pending,
        "Approved" => ApprovalState::Approved,
        "Rejected" => ApprovalState::Rejected,
        _ => return Err(invalid("dreaming_proposal_state").into()),
    };
    Ok(ProposalRecord {
        id: row.id.clone(),
        revision: rev(row.revision),
        content_digest: Digest::try_from(row.content_digest.clone())
            .map_err(|_| invalid("dreaming_proposal_digest"))?,
        state,
        operation_kind: label(&row.operation_kind, PROPOSAL_OPERATION),
        target_id: row.target_id.clone(),
        target_revision: rev(row.target_revision),
        summary: content(&row.summary),
        artifact: row
            .artifact
            .clone()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|_| invalid("dreaming_proposal_artifact"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejection_requires_new_evidence_observed_after_the_decision() {
        let prior = [json!({"kind":"entry","id":"target","fingerprint":"original"})];
        assert!(!has_fresh_evidence(&prior, vec![102], &prior, 100));
        let older = [json!({"kind":"review","id":"previously-uncited","fingerprint":"old"})];
        assert!(!has_fresh_evidence(&older, vec![99], &prior, 100));
        assert!(!has_fresh_evidence(&older, vec![100], &prior, 100));
        let fresh = [json!({"kind":"review","id":"new-review","fingerprint":"new"})];
        assert!(has_fresh_evidence(&fresh, vec![101], &prior, 100));
    }
}
