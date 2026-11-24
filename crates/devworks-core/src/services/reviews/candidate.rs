//! A capped review may qualify an exact repair while the original head stays attributed.
use super::commands::refused;
use super::policy::{
    CandidateRepair, ChangeDigest, CurrentCandidate, FindingSeverity, FocusedCheck, RepairReason,
    RequiredCheckFailure, ReviewStage,
};
use super::{ReviewService, ReviewStateRecord, ReviewTarget, decode};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedCommand;
use crate::operations::reviews::RequestInput;
use crate::ports::storage::{
    GithubScope, NewEvent, StorageError, StoredGitWorktree, StoredGithubObservation,
    WriteTransaction,
};
use crate::ports::{IdKind, Preparation};
use crate::protocol::data::{DeliveryKind, GitOid};
use crate::protocol::records::{
    CodeReviewSeverity, LocalReviewDiff, ReviewCandidateRepair, ReviewChangeReason, ReviewFinding,
    ReviewObservationState,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{DomainErrorCode, EventId, Id};
use crate::services::github::code_review::DeliveryReviewRules;
use crate::services::kernel::{Failure, not_found};
use serde::Deserialize;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VerificationReport {
    repository_id: Id,
    original_review_id: Id,
    reviewed_head: GitOid,
    candidate_head: GitOid,
    preserves_product_behavior: bool,
    preserves_coverage: bool,
    preserves_required_gates: bool,
    rationale: String,
    focused_checks: Vec<VerificationCheck>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VerificationCheck {
    id: Id,
    findings_verified: Vec<Id>,
}

pub(super) struct PreparedCandidate {
    tree: StoredGitWorktree,
    repository_revision: i64,
    changes: BTreeSet<ChangeDigest>,
    report: VerificationReport,
    rules: DeliveryReviewRules,
}

impl ReviewService {
    pub(super) fn prepare_candidate(
        &self,
        ctx: &InvocationContext,
        proof: &ReviewCandidateRepair,
        env: &Preparation<'_>,
    ) -> Result<PreparedCandidate, Failure> {
        let (row, state) = env.read(|snapshot| {
            Ok(super::visible_review(
                snapshot,
                ctx,
                &proof.original_review_id,
            ))
        })??;
        let project = row.project_id;
        let pull = state
            .final_rounds
            .iter()
            .find(|round| round.repository_id == proof.repository_id)
            .map(|round| round.pull_id.clone())
            .ok_or(StorageError::ScopeDenied)?;
        let worktree = env.read(|snapshot| {
            let link = snapshot
                .sessions()
                .link(&pull)?
                .ok_or(StorageError::ScopeDenied)?;
            serde_json::from_value::<Id>(link.body["worktreeId"].clone())
                .map_err(|_| StorageError::ScopeDenied)
        })?;
        let (tree, repository_revision, manifest) = self.git.capture_candidate_changes(
            ctx,
            env,
            &LocalReviewDiff {
                repository_id: proof.repository_id.clone(),
                worktree_id: worktree,
                base: proof.reviewed_head.clone(),
                head: proof.candidate_head.clone(),
            },
        )?;
        let changes = manifest["files"]
            .as_array()
            .ok_or(StorageError::ScopeDenied)?
            .iter()
            .map(|file| {
                let name = file["path"].as_str().ok_or(StorageError::ScopeDenied)?;
                let patch = file["sha256"].as_str().ok_or(StorageError::ScopeDenied)?;
                let digest = crate::fingerprint::canonical_digest(&json!([name, patch]))
                    .map_err(|_| StorageError::InvalidRecord("candidate patch digest".into()))?;
                Ok(ChangeDigest(digest))
            })
            .collect::<Result<BTreeSet<_>, StorageError>>()?;
        let (bytes, _) = self
            .artifacts
            .read_exact(
                self.store.as_ref(),
                ctx,
                &proof.verification_report,
                256 * 1024,
            )
            .map_err(|_| {
                refused(
                    DomainErrorCode::SourceChanged,
                    "verification_report",
                    &proof.verification_report.artifact_id,
                )
            })?;
        let report = serde_json::from_slice::<VerificationReport>(&bytes).map_err(|_| {
            refused(
                DomainErrorCode::InvalidArgument,
                "verification_report",
                &proof.verification_report.artifact_id,
            )
        })?;
        let rules = self
            .github
            .as_ref()
            .ok_or_else(|| {
                refused(
                    DomainErrorCode::UnsupportedCapability,
                    "required_github_checks",
                    &proof.repository_id,
                )
            })?
            .delivery_review_rules(&project, &proof.repository_id)?;
        Ok(PreparedCandidate {
            tree,
            repository_revision,
            changes,
            report,
            rules,
        })
    }

    pub(super) fn verify_candidate(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        input: &RequestInput,
        proof: &ReviewCandidateRepair,
    ) -> Result<DomainDecision, Failure> {
        let root = crate::services::work::visible_work(tx.work(), ctx, &input.root_id.0)?;
        crate::services::work::open_project(tx, ctx, &root.project_id)?;
        let invalid = || {
            refused(
                DomainErrorCode::SourceChanged,
                "review_candidate",
                &proof.repository_id,
            )
        };
        if root.revision != input.expected_revision.get()
            || &input.policy_revision != ctx.policy_revision()
            || ctx
                .root_id()
                .is_some_and(|current| current != &input.root_id)
        {
            return Err(invalid());
        }
        if ctx.principal_kind() != PrincipalKind::Human {
            let role = ctx
                .assignment_id()
                .and_then(|id| tx.runs().agent_assignment_context(id).ok().flatten())
                .and_then(|owner| {
                    crate::settings::catalog::AgentRole::parse(owner.role_id.as_str())
                });
            if ctx.root_id() != Some(&input.root_id)
                || !matches!(
                    role,
                    Some(
                        crate::settings::catalog::AgentRole::FeatureCoordinator
                            | crate::settings::catalog::AgentRole::IssueCoordinator
                    )
                )
            {
                return Err(StorageError::ScopeDenied.into());
            }
        }
        let prepared = command
            .prepared::<Result<PreparedCandidate, Failure>>()
            .ok_or_else(invalid)?
            .as_ref()
            .map_err(Clone::clone)?;
        crate::services::work::check_artifact(
            tx,
            ctx,
            &root.project_id,
            &proof.verification_report,
        )?;
        let artifact = tx
            .artifacts()
            .artifact(&proof.verification_report.artifact_id)?
            .filter(|artifact| artifact.owner.as_ref() == Some(ctx.principal()))
            .ok_or_else(invalid)?;
        if artifact.scope.project() != Some(&root.project_id)
            || tx.git().git_worktree(&prepared.tree.id)?.as_ref() != Some(&prepared.tree)
            || tx
                .projects()
                .repository(&proof.repository_id)?
                .is_none_or(|repo| {
                    repo.revision != prepared.repository_revision || repo.lifecycle != "Active"
                })
            || !tx
                .projects()
                .members(&root.project_id)?
                .contains(&proof.repository_id)
        {
            return Err(invalid());
        }
        let mut row = tx
            .git()
            .code_review(&proof.original_review_id)?
            .filter(|row| row.root_id == input.root_id && row.project_id == root.project_id)
            .ok_or_else(|| not_found("original_review", &proof.original_review_id))?;
        if row.revision != proof.expected_review_revision.get() {
            return Err(invalid());
        }
        let mut saved: ReviewStateRecord = decode(&row.body)?;
        let ReviewTarget::PullRequests { barrier } = &saved.target else {
            return Err(invalid());
        };
        let stage = match barrier.kind {
            DeliveryKind::Feature => ReviewStage::FinalFeature,
            DeliveryKind::StandaloneIssue => ReviewStage::StandaloneIssue,
        };
        let round = saved
            .final_rounds
            .iter()
            .find(|round| round.repository_id == proof.repository_id)
            .ok_or_else(invalid)?;
        let source = saved
            .value
            .sources
            .0
            .iter()
            .find(|source| {
                source.repository_id == proof.repository_id
                    && source.head == proof.reviewed_head
                    && matches!(source.state, ReviewObservationState::Completed)
            })
            .ok_or_else(invalid)?;
        let settings = crate::settings::service::review_settings(
            tx.configuration(),
            &root.project_id,
            &proof.repository_id,
        )?;
        if saved.revision_of_policy != input.policy_revision.get()
            || settings.digest != round.settings_digest
            || round
                .delivery_rules
                .as_ref()
                .is_none_or(|rules| rules.digest != prepared.rules.digest)
        {
            return Err(invalid());
        }

        let lineage = super::final_review::review_lineage(
            tx,
            &row.project_id,
            &row.root_id,
            &proof.repository_id,
            &round.pull_id,
        )?;
        if lineage.iter().any(|state| {
            state.final_rounds.iter().any(|prior| {
                prior.repository_id == proof.repository_id
                    && prior.pull_id == round.pull_id
                    && prior.cycle > round.cycle
            })
        }) {
            return Err(invalid());
        }

        let report = &prepared.report;
        if report.repository_id != proof.repository_id
            || report.original_review_id != proof.original_review_id
            || report.reviewed_head != proof.reviewed_head
            || report.candidate_head != proof.candidate_head
            || report.rationale.trim().is_empty()
            || report.rationale.len() > 4096
            || report.focused_checks.len() != proof.focused_check_ids.0.len()
        {
            return Err(invalid());
        }
        let scope = GithubScope {
            repositories: vec![proof.repository_id.clone()],
            project_id: Some(root.project_id.clone()),
        };
        let mut failed = HashMap::new();
        let mut diagnoses = HashMap::new();
        for item in &proof.diagnoses.0 {
            let evidence = tx
                .github()
                .github_observation(&item.failed_check_id, &scope)?
                .filter(|observation| {
                    observation.head.as_deref() == Some(&proof.reviewed_head.hex)
                        && failing_required_check(observation, &prepared.rules)
                })
                .ok_or_else(invalid)?;
            if failed
                .insert(
                    evidence.id.clone(),
                    RequiredCheckFailure {
                        head: proof.reviewed_head.clone(),
                        required: true,
                    },
                )
                .is_some()
            {
                return Err(invalid());
            }
            diagnoses.insert(evidence.id, item.diagnosis.0.clone());
        }
        let mut checks = HashMap::new();
        for statement in &report.focused_checks {
            let evidence = tx
                .github()
                .github_observation(&statement.id, &scope)?
                .filter(|observation| {
                    observation.head.as_deref() == Some(&proof.candidate_head.hex)
                        && passed_check(observation)
                })
                .ok_or_else(invalid)?;
            if checks
                .insert(
                    evidence.id,
                    FocusedCheck {
                        head: proof.candidate_head.clone(),
                        passed: true,
                        preserves_product_behavior: report.preserves_product_behavior,
                        preserves_coverage: report.preserves_coverage,
                        preserves_required_gates: report.preserves_required_gates,
                        findings_verified: statement.findings_verified.iter().cloned().collect(),
                    },
                )
                .is_some()
            {
                return Err(invalid());
            }
        }
        let mut findings = HashMap::new();
        for id in lineage
            .iter()
            .flat_map(|review| &review.value.finding_ids.0)
        {
            let finding = tx.git().code_finding(id)?.ok_or_else(invalid)?;
            if finding.repository_id.as_ref() != Some(&proof.repository_id) {
                continue;
            }
            let value: ReviewFinding = decode(&finding.body)?;
            if value.actionable {
                let severity = match value.code_severity {
                    Some(CodeReviewSeverity::Blocker) => FindingSeverity::Blocker,
                    Some(CodeReviewSeverity::Major) => FindingSeverity::Major,
                    Some(CodeReviewSeverity::Informational) => FindingSeverity::Informational,
                    _ => FindingSeverity::Minor,
                };
                findings.insert(id.clone(), severity);
            }
        }
        let classified = proof
            .changes
            .0
            .iter()
            .map(|change| {
                let reason = match &change.reason {
                    ReviewChangeReason::RequiredCheck { failed_check_id } => {
                        RepairReason::RequiredCheck {
                            failed_check_id: failed_check_id.clone(),
                        }
                    }
                    ReviewChangeReason::Finding { finding_id } => RepairReason::Finding {
                        finding_id: finding_id.clone(),
                    },
                };
                (ChangeDigest(change.change_digest.0.clone()), reason)
            })
            .collect::<BTreeMap<_, _>>();
        if classified.len() != proof.changes.0.len() {
            return Err(invalid());
        }
        let focused_check_ids: HashSet<_> = proof.focused_check_ids.0.iter().cloned().collect();
        if focused_check_ids.len() != checks.len()
            || focused_check_ids.len() != proof.focused_check_ids.0.len()
        {
            return Err(invalid());
        }
        let proposed = CandidateRepair {
            reviewed_head: proof.reviewed_head.clone(),
            candidate_head: proof.candidate_head.clone(),
            diagnosed_failed_checks: diagnoses,
            changes: classified,
            focused_check_ids,
        };
        proposed
            .validate(&CurrentCandidate {
                stage,
                configured_cap: Some(settings.cap),
                admitted_rounds: round.cycle,
                reviewed_head: &source.head,
                head: &prepared.tree.head,
                complete_delta: &prepared.changes,
                failed_checks: &failed,
                actionable_findings: &findings,
                focused_checks: &checks,
                requires_fresh_review: prepared.rules.fresh_review,
            })
            .map_err(|fault| {
                let kind = match fault {
                    super::policy::RepairFault::FreshReviewRequired => "fresh_review_required",
                    super::policy::RepairFault::IncompleteDelta => "review_candidate_delta",
                    _ => "review_candidate_evidence",
                };
                refused(DomainErrorCode::Conflict, kind, &proof.repository_id)
            })?;
        saved
            .verified_candidates
            .retain(|earlier| earlier.repository_id != proof.repository_id);
        saved.verified_candidates.push(proof.clone());
        saved.value.candidate_repair = Some(proof.clone());
        super::remote::update_final_state(tx, &mut row, &mut saved)?;
        let id = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent {
            id,
            actor: ctx.principal().clone(),
            project_id: Some(root.project_id),
            root_id: Some(input.root_id.clone()),
            operation_id: None,
            aggregate_kind: "code_review".into(),
            aggregate_id: row.id.as_str().into(),
            aggregate_revision: row.revision,
            kind: "CodeReviewCandidateVerified".into(),
            payload: json!({"proof":proof,"branchRulesDigest":prepared.rules.digest}),
        })?;
        Ok(DomainDecision::Applied {
            result: OperationResult::ReviewsRequest { value: saved.value },
        })
    }
}

fn failing_required_check(
    observation: &StoredGithubObservation,
    rules: &DeliveryReviewRules,
) -> bool {
    let check = &observation.payload;
    let failed = match observation.kind.as_str() {
        "CheckRun" => {
            check["state"] == "completed"
                && matches!(
                    check["conclusion"].as_str(),
                    Some("failure" | "timed_out" | "action_required")
                )
        }
        "CommitStatus" => matches!(check["state"].as_str(), Some("failure" | "error")),
        _ => false,
    };
    failed
        && rules.required_checks.iter().any(|(name, app)| {
            check["name"].as_str() == Some(name)
                && app.is_none_or(|app| check["appId"].as_u64() == Some(app))
        })
}

pub(super) fn passed_check(observation: &StoredGithubObservation) -> bool {
    match observation.kind.as_str() {
        "CheckRun" => {
            observation.payload["state"] == "completed"
                && observation.payload["conclusion"] == "success"
        }
        "CommitStatus" => observation.payload["state"] == "success",
        _ => false,
    }
}
