//! Internal consolidation only. The caller is the checked Dreaming run lease. Public Knowledge
//! and approval commands keep their existing authority, provenance and receipt paths.
use super::writes;
use crate::context::InvocationContext;
use crate::operations::knowledge as ops;
use crate::ports::storage::{
    DreamRunRow, DreamTrailQuery, DreamingRead, KnowledgeScope, StorageError, StoredClaim,
    WriteTransaction,
};
use crate::protocol::records::{
    Applicability, ClaimDraft, EvidenceKind, EvidenceLocator, KnowledgeEntryKind, KnowledgeOrigin,
    LessonSource, PromoteAction, SourceEvidence,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Check};
use crate::protocol::{Counter, DomainErrorCode, Id, Revision};
use crate::services::kernel::{Failure, Refusal};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) enum Consolidation {
    Record(Box<ClaimDraft>, Vec<SourceEvidence>),
    Relate(ops::RelateInput),
    Promote(ops::PromoteInput),
    Verify(ops::VerifyInput, std::collections::BTreeMap<String, String>),
    Challenge {
        input: ops::ChallengeInput,
        challenge_id: Id,
    },
    Applicability {
        target: Id,
        expected: Revision,
        applicable: Applicability,
    },
}

pub(crate) fn is_write(tool: &str) -> bool {
    matches!(
        tool,
        "dream_record"
            | "dream_relate"
            | "dream_promote"
            | "dream_verify"
            | "dream_challenge"
            | "dream_revise"
    )
}

pub(crate) fn apply(
    tx: &mut dyn WriteTransaction,
    run: &DreamRunRow,
    tool: &str,
    args: &Value,
    refreshed: &BTreeSet<String>,
) -> Result<Value, Failure> {
    let proofs = load_proofs(tx.dreaming()?, run, &args["evidence"])?;
    for proof in &proofs {
        match proof["kind"].as_str() {
            Some("graph_node") if !current_graph_proof(tx, run, proof, refreshed)? => {
                return Err(invalid("dreaming_source_changed"));
            }
            Some("entry") => {
                let current = target(tx, &run.scope, &proof["id"])?;
                if Some(current.revision) != proof["revision"].as_i64() {
                    return Err(invalid("dreaming_evidence_changed"));
                }
            }
            Some("lesson_event" | "challenge") => {
                let current = target(tx, &run.scope, &proof["claimId"])?;
                if Some(current.revision) != proof["revision"].as_i64() || current.lesson.is_none()
                {
                    return Err(invalid("dreaming_lesson_evidence_changed"));
                }
            }
            Some("review" | "thread" | "message" | "attempt" | "handoff")
                if !current_work_proof(tx, run, proof)? =>
            {
                return Err(invalid("dreaming_work_evidence_changed"));
            }
            _ => {}
        }
    }
    let evidence: Vec<Id> = proofs
        .iter()
        .filter_map(|proof| proof["evidenceId"].as_str())
        .map(Id::new)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid("dreaming_evidence"))?;
    let scope = &run.scope;
    let mut before = Vec::new();
    let mut opened_challenge = None;
    let action = if tool == "dream_record" {
        let mut draft: ClaimDraft = serde_json::from_value(args["entry"].clone())
            .map_err(|_| invalid("dreaming_record"))?;
        draft.check().map_err(|_| invalid("dreaming_record"))?;
        if !matches!(
            draft.entry_kind,
            Some(
                KnowledgeEntryKind::Fact
                    | KnowledgeEntryKind::Gotcha
                    | KnowledgeEntryKind::Constraint
                    | KnowledgeEntryKind::Attempt
                    | KnowledgeEntryKind::Lesson
            )
        ) || draft.supersedes.is_some()
            || draft
                .origin
                .as_ref()
                .is_some_and(|origin| !matches!(origin, KnowledgeOrigin::Agent))
            || draft.lesson.as_ref().is_some_and(|lesson| {
                !matches!(lesson.source, LessonSource::Agent)
                    || lesson.quoted_words.is_some()
                    || lesson.same_as.is_some()
            })
        {
            return Err(StorageError::ScopeDenied.into());
        }
        draft.origin = Some(KnowledgeOrigin::Agent);
        draft.origin_ref = Some(BoundedText(run.id.as_str().into()));
        let sources = proofs
            .iter()
            .enumerate()
            .map(|(index, proof)| citation(index, proof))
            .collect::<Result<Vec<_>, _>>()?;
        let mut evidence = evidence;
        evidence.extend(sources.iter().map(|citation| citation.id.clone()));
        draft.evidence_ids = BoundedList(evidence);
        Consolidation::Record(Box::new(draft), sources)
    } else {
        let target = target(tx, scope, &args["id"])?;
        let expected = args["expected_version"].as_i64().unwrap_or(target.revision);
        if expected != target.revision
            || !proofs.iter().any(|proof| {
                proof["id"] == json!(target.id)
                    && proof["revision"].as_i64() == Some(target.revision)
            })
        {
            return Err(invalid("dreaming_target_evidence"));
        }
        let expected = Revision::new(expected).map_err(|_| invalid("dreaming_target_revision"))?;
        let mut targets = vec![target.clone()];
        let action = match tool {
            "dream_verify" => {
                if target.basis.is_empty()
                    || target.basis.iter().any(|(source, _)| {
                        !proofs.iter().any(|proof| {
                            proof["sourceRef"] == json!(source)
                                && proof["verifiedIndexMatch"].as_bool() == Some(true)
                        })
                    })
                {
                    return Err(invalid("dreaming_verification_proof"));
                }
                let mut revisions = std::collections::BTreeMap::new();
                for (source, _) in &target.basis {
                    let proof = proofs
                        .iter()
                        .find(|proof| {
                            proof["sourceRef"] == json!(source)
                                && proof["verifiedIndexMatch"] == true
                        })
                        .and_then(|proof| proof["sourceRevision"].as_str())
                        .ok_or_else(|| invalid("dreaming_source_changed"))?;
                    let current = tx.knowledge().knowledge_source_head(source)?;
                    if current.as_deref().is_some_and(|known| known != proof) {
                        return Err(invalid("dreaming_source_changed"));
                    }
                    revisions.insert(source.clone(), proof.to_owned());
                }
                Consolidation::Verify(
                    ops::VerifyInput {
                        scope: scope.into(),
                        claim_id: target.id,
                        expected_revision: expected,
                        reason: reason(args, "prevalence")?,
                        evidence_ids: BoundedList(evidence),
                        source_revision: None,
                    },
                    revisions,
                )
            }
            "dream_revise" => {
                enforce_editable_lesson(&target)?;
                let applicable: Applicability =
                    serde_json::from_value(args["applies_when"].clone())
                        .map_err(|_| invalid("lesson_applicability"))?;
                applicable
                    .check()
                    .map_err(|_| invalid("lesson_applicability"))?;
                Consolidation::Applicability {
                    target: target.id,
                    expected,
                    applicable,
                }
            }
            "dream_promote" => {
                enforce_editable_lesson(&target)?;
                Consolidation::Promote(ops::PromoteInput {
                    claim_id: target.id,
                    expected_revision: expected,
                    destination: scope.into(),
                    proposal_id: None,
                    action: Some(PromoteAction::Propose),
                    address: None,
                    reason: Some(reason(args, "prevalence")?),
                })
            }
            "dream_challenge" => {
                if target
                    .lesson
                    .as_ref()
                    .is_none_or(|lesson| lesson.rule_state != "Effective")
                {
                    return Err(invalid("dreaming_challenge_requires_effective_rule"));
                }
                let challenge_id = tx.new_id(crate::ports::IdKind::KnowledgeChallenge);
                opened_challenge = Some(challenge_id.clone());
                Consolidation::Challenge {
                    input: ops::ChallengeInput {
                        scope: scope.into(),
                        claim_id: target.id,
                        expected_revision: expected,
                        reason: reason(args, "reason")?,
                        evidence_ids: BoundedList(evidence),
                        resolve_challenge_id: None,
                        outcome: None,
                    },
                    challenge_id,
                }
            }
            "dream_relate" => {
                let duplicate = args["same_as"].is_string();
                let other_id = if duplicate {
                    &args["same_as"]
                } else {
                    &args["contradicts"]
                };
                let other = self::target(tx, scope, other_id)?;
                if !proofs.iter().any(|proof| {
                    proof["id"] == json!(other.id)
                        && proof["revision"].as_i64() == Some(other.revision)
                }) {
                    return Err(invalid("dreaming_other_target_evidence"));
                }
                if duplicate {
                    enforce_editable_lesson(&target)?;
                    enforce_editable_lesson(&other)?;
                    // An ambiguity is retained as review work. Only a corroborated duplicate
                    // with matching normalized content and target can take effect here.
                    if (
                        target.subject_id.clone(),
                        &target.predicate,
                        &target.object_id,
                    ) != (other.subject_id.clone(), &other.predicate, &other.object_id)
                        || !same_claim_text(&target, &other)
                    {
                        return Ok(
                            json!({"reviewRequired":true,"reason":"DuplicateEvidenceConflicts","otherRevision":other.revision}),
                        );
                    }
                } else if target.lesson.is_none() || other.lesson.is_none() {
                    return Err(invalid("dreaming_relate_requires_lessons"));
                }
                targets.push(other.clone());
                Consolidation::Relate(ops::RelateInput {
                    scope: scope.into(),
                    subject_id: target.id,
                    object_id: other.id,
                    predicate: BoundedText(
                        if duplicate { "same_as" } else { "contradicts" }.into(),
                    ),
                    evidence_ids: BoundedList(evidence),
                })
            }
            _ => return Err(invalid("dreaming_action")),
        };
        before = targets;
        action
    };
    let ctx = native_context(tx, run)?;
    let result = writes::consolidate(tx, &ctx, scope, action)?;
    let (result, changed) = match result {
        DomainDecision::Applied { result } => (result, true),
        DomainDecision::NoChange { result } => (result, false),
        _ => return Err(invalid("dreaming_knowledge_result")),
    };
    let receipt = match result {
        OperationResult::KnowledgeRecord { value }
        | OperationResult::KnowledgeRelate { value }
        | OperationResult::KnowledgeRevise { value }
        | OperationResult::KnowledgeVerify { value }
        | OperationResult::KnowledgePromote { value }
        | OperationResult::KnowledgeChallenge { value } => value,
        _ => return Err(invalid("dreaming_knowledge_result")),
    };
    let write = tx
        .knowledge()
        .knowledge_write(&receipt.write_id)?
        .ok_or_else(|| invalid("dreaming_knowledge_receipt"))?;
    Ok(
        json!({"changed":changed,"receipt":receipt,"before":before,"changes":write.changes,
        "evidence":proofs,"prevalence":args["prevalence"],"challengeId":opened_challenge}),
    )
}

fn same_claim_text(left: &StoredClaim, right: &StoredClaim) -> bool {
    let normalized = |claim: &StoredClaim| {
        claim
            .body
            .normalized_text
            .as_ref()
            .or(claim.body.value.as_ref())
            .cloned()
    };
    normalized(left).is_some_and(|text| Some(text) == normalized(right))
        && left.body.value_json == right.body.value_json
}

fn native_context(
    tx: &mut dyn WriteTransaction,
    run: &DreamRunRow,
) -> Result<InvocationContext, Failure> {
    let info = tx.store();
    let epoch = Counter::new(info.restore_epoch).map_err(|_| invalid("store_epoch"))?;
    Ok(InvocationContext::new(
        info.host_actor.clone(),
        crate::context::PrincipalKind::Host,
        run.id.clone(),
        run.id.clone(),
        info.store_id.clone(),
        epoch,
        Revision::new(run.settings_revision).map_err(|_| invalid("dreaming_settings_revision"))?,
    )
    .with_lineage(run.scope.project().cloned(), None, None)
    .with_session(run.id.clone()))
}

fn target(
    tx: &mut dyn WriteTransaction,
    scope: &KnowledgeScope,
    raw: &Value,
) -> Result<StoredClaim, Failure> {
    let id = raw
        .as_str()
        .and_then(|value| Id::new(value).ok())
        .ok_or_else(|| invalid("dreaming_target"))?;
    tx.knowledge()
        .knowledge_claim(&id)?
        .filter(|claim| &claim.scope == scope && claim.lifecycle == "Active")
        .ok_or_else(|| invalid("dreaming_target_unavailable"))
}

fn enforce_editable_lesson(claim: &StoredClaim) -> Result<(), Failure> {
    if claim.lesson.as_ref().is_none_or(|lesson| {
        matches!(
            lesson.source.as_str(),
            "Prompt" | "Review" | "RelayedPrompt" | "RelayedReview"
        )
    }) {
        return Err(StorageError::ScopeDenied.into());
    }
    Ok(())
}

fn reason(args: &Value, value: &str) -> Result<BoundedText<0, 65536>, Failure> {
    args[value]
        .as_str()
        .map(|text| BoundedText(text.into()))
        .ok_or_else(|| invalid("dreaming_reason"))
}

pub(crate) fn load_proofs(
    repo: &dyn DreamingRead,
    run: &DreamRunRow,
    requested: &Value,
) -> Result<Vec<Value>, Failure> {
    let Some(requests) = requested.as_array().filter(|items| !items.is_empty()) else {
        return Err(invalid("dreaming_evidence"));
    };
    let mut wanted = BTreeSet::new();
    for item in requests {
        let Some((kind, reference)) = item["kind"].as_str().zip(item["reference"].as_str()) else {
            return Err(invalid("dreaming_evidence"));
        };
        if !wanted.insert((kind.to_owned(), reference.to_owned())) {
            return Err(invalid("dreaming_duplicate_evidence"));
        }
    }
    let mut proofs = Vec::new();
    let mut before = None;
    while !wanted.is_empty() {
        let page = repo.dream_actions(&run.id, before, 100)?;
        if page.is_empty() {
            break;
        }
        before = page.last().map(|row| row.position);
        for row in page {
            let Some(items) = row.action["result"]["proofs"].as_array() else {
                continue;
            };
            for proof in items {
                let Some(key) = proof["kind"].as_str().zip(proof["id"].as_str()) else {
                    continue;
                };
                if wanted.remove(&(key.0.to_owned(), key.1.to_owned())) {
                    proofs.push(proof.clone());
                }
            }
        }
    }
    if !wanted.is_empty() {
        return Err(invalid("dreaming_evidence_not_read_in_this_run"));
    }
    Ok(proofs)
}

fn citation(index: usize, proof: &Value) -> Result<SourceEvidence, Failure> {
    let kind = proof["kind"]
        .as_str()
        .ok_or_else(|| invalid("dreaming_proof_kind"))?;
    let reference = proof["id"]
        .as_str()
        .ok_or_else(|| invalid("dreaming_proof_id"))?;
    let fingerprint = proof["fingerprint"]
        .as_str()
        .ok_or_else(|| invalid("dreaming_proof_fingerprint"))?;
    let source = if let Some(source) = proof["sourceRef"].as_str() {
        source.to_owned()
    } else {
        format!("{kind}:{reference}")
    };
    let id =
        Id::new(format!("dream-evidence-{index}")).map_err(|_| invalid("dreaming_evidence_id"))?;
    Ok(SourceEvidence {
        id,
        claim_id: None,
        source: EvidenceLocator {
            kind: if kind == "graph_node" {
                EvidenceKind::Code
            } else {
                EvidenceKind::Observation
            },
            source_ref: BoundedText(source),
            artifact: None,
            locator: Some(BoundedText(format!("sha256:{fingerprint}"))),
        },
        snippet: BoundedText(format!("{kind} {reference}")),
        metadata: None,
        source_revision: proof["sourceRevision"]
            .as_str()
            .map(|revision| BoundedText(revision.to_owned()))
            .or_else(|| {
                proof["revision"]
                    .as_i64()
                    .map(|revision| BoundedText(revision.to_string()))
            }),
    })
}

pub(crate) fn current_work_proof(
    tx: &mut dyn WriteTransaction,
    run: &DreamRunRow,
    proof: &Value,
) -> Result<bool, Failure> {
    Ok(work_proof_observed_at(tx, run, proof)?.is_some())
}

pub(crate) fn work_proof_observed_at(
    tx: &mut dyn WriteTransaction,
    run: &DreamRunRow,
    proof: &Value,
) -> Result<Option<i64>, Failure> {
    let (Some(kind), Some(reference), Some(fingerprint)) = (
        proof["kind"].as_str(),
        proof["id"].as_str(),
        proof["fingerprint"].as_str(),
    ) else {
        return Ok(None);
    };
    let exact = tx.dreaming()?.dream_trail(&DreamTrailQuery {
        scope: &run.scope,
        through_ms: run.sweep_started_at_ms(),
        knowledge_through: run.cutoffs.knowledge,
        reviews_through: run.cutoffs.reviews,
        since_ms: None,
        kind: Some(kind),
        id: Some(reference),
        item_id: None,
        before: None,
        offset: 0,
        limit: 1,
    })?;
    if let Some(row) = exact.first() {
        let canonical = crate::jcs::canonicalize(&row.content)
            .map_err(|_| invalid("dreaming_work_evidence"))?;
        return Ok(
            (crate::fingerprint::sha256_hex(canonical.as_bytes()) == fingerprint)
                .then_some(row.position.at_ms),
        );
    }
    Ok(None)
}

pub(crate) fn current_graph_proof(
    tx: &mut dyn WriteTransaction,
    run: &DreamRunRow,
    proof: &Value,
    refreshed: &BTreeSet<String>,
) -> Result<bool, Failure> {
    let Some(fingerprint) = proof["fingerprint"].as_str() else {
        return Ok(false);
    };
    let Some(reference) = proof["id"].as_str() else {
        return Ok(false);
    };
    if !refreshed.contains(&format!("{reference}:{fingerprint}")) {
        return Ok(false);
    }
    Ok(graph_proof_observed_at(tx, run, proof)?.is_some())
}

pub(crate) fn graph_proof_observed_at(
    tx: &mut dyn WriteTransaction,
    run: &DreamRunRow,
    proof: &Value,
) -> Result<Option<i64>, Failure> {
    let repository: Id = serde_json::from_value(proof["repositoryId"].clone())
        .map_err(|_| invalid("dreaming_proof_repository"))?;
    let Some(project) = run.scope.project() else {
        return Ok(None);
    };
    if !tx
        .projects()
        .repository_projects(&repository)?
        .contains(project)
        || !tx.projects().repository(&repository)?.is_some_and(|repo| {
            repo.lifecycle == "Active"
                && Some(repo.revision) == proof["repositoryRevision"].as_i64()
        })
    {
        return Ok(None);
    }
    if !tx
        .indexing()
        .published(&repository)?
        .is_some_and(|view| json!(view.view_id) == proof["viewId"])
    {
        return Ok(None);
    }
    let capture: Id = serde_json::from_value(proof["captureId"].clone())
        .map_err(|_| invalid("dreaming_proof_capture"))?;
    Ok(tx
        .graph()
        .graph_source_capture(&capture)?
        .filter(|capture| {
            capture.repository_id == repository
                && proof["unitKey"] == capture.unit_key
                && proof["fingerprint"] == capture.blob_digest
                && proof["sourceRevision"] == capture.source_digest
        })
        .map(|capture| capture.created_at_ms))
}

fn invalid(kind: &'static str) -> Failure {
    Refusal::new(DomainErrorCode::InvalidArgument, kind, None).into()
}
