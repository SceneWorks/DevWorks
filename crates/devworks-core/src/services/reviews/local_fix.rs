//! Verification of a changed local candidate retains the one original independent review.
use super::commands::refused;
use super::{ReviewService, ReviewStateRecord, ReviewTarget};
use crate::context::InvocationContext;
use crate::gateway::AdmittedCommand;
use crate::operations::reviews::AdjudicateInput;
use crate::ports::Preparation;
use crate::ports::storage::{StorageError, StoredCodeFinding, StoredGitWorktree, WriteTransaction};
use crate::protocol::data::GitOid;
use crate::protocol::provider::ArtifactRef;
use crate::protocol::records::CodeReviewResolution;
use crate::protocol::{DomainErrorCode, Id, OperationId, OperationState, RootId};
use crate::services::kernel::Failure;
use serde::Deserialize;
use std::collections::HashSet;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Report {
    review_id: Id,
    finding_id: Id,
    candidate_head: GitOid,
    focused_check_ids: Vec<Id>,
    rationale: String,
}

pub(super) struct PreparedLocalFix {
    git_tree: StoredGitWorktree,
    repository_revision: i64,
    evidence: ArtifactRef,
}

impl ReviewService {
    pub(super) fn prepare_local_fix(
        &self,
        ctx: &InvocationContext,
        input: &AdjudicateInput,
        env: &Preparation<'_>,
    ) -> Result<Option<PreparedLocalFix>, Failure> {
        let Some(CodeReviewResolution::FixVerified {
            candidate_head,
            focused_check_ids,
        }) = input.resolution.as_ref()
        else {
            return Ok(None);
        };
        let (finding, review) = env.read(|snapshot| {
            let finding = snapshot
                .git()
                .code_finding(&input.finding_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let (_, review) = super::visible_review(snapshot, ctx, &finding.review_id)
                .map_err(Failure::into_storage)?;
            Ok((finding, review))
        })?;
        let ReviewTarget::Local { target } = review.target else {
            return Ok(None);
        };
        let invalid = || {
            refused(
                DomainErrorCode::SourceChanged,
                "local_review_fix",
                &finding.id,
            )
        };
        let mut source = target
            .diffs
            .0
            .iter()
            .find(|diff| finding.repository_id.as_ref() == Some(&diff.repository_id))
            .cloned()
            .ok_or_else(invalid)?;
        source.base = source.head;
        source.head = candidate_head.clone();
        let (git_tree, repository_revision, manifest) =
            self.git.capture_candidate_changes(ctx, env, &source)?;
        if manifest["files"].as_array().is_none_or(Vec::is_empty) {
            return Err(invalid());
        }
        let expected: HashSet<&Id> = focused_check_ids.0.iter().collect();
        if expected.is_empty() || expected.len() != focused_check_ids.0.len() {
            return Err(invalid());
        }
        let verified = input
            .evidence
            .0
            .iter()
            .find(|reference| {
                self.artifacts
                    .read_exact(self.store.as_ref(), ctx, reference, 64 * 1024)
                    .ok()
                    .and_then(|(bytes, _)| serde_json::from_slice::<Report>(&bytes).ok())
                    .is_some_and(|report| {
                        report.review_id == review.value.id
                            && report.finding_id == finding.id
                            && &report.candidate_head == candidate_head
                            && report.focused_check_ids.iter().collect::<HashSet<_>>() == expected
                            && !report.rationale.trim().is_empty()
                            && report.rationale.len() <= 4096
                    })
            })
            .ok_or_else(invalid)?;
        Ok(Some(PreparedLocalFix {
            git_tree,
            repository_revision,
            evidence: verified.clone(),
        }))
    }
}

pub(super) fn qualified(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    command: &AdmittedCommand,
    finding: &StoredCodeFinding,
    state: &ReviewStateRecord,
    candidate_head: &GitOid,
    focused_check_ids: &[Id],
) -> Result<bool, Failure> {
    let ReviewTarget::Local { target } = &state.target else {
        return Ok(false);
    };
    let Some(prepared) = command
        .prepared::<Result<Option<PreparedLocalFix>, Failure>>()
        .map(|prepared| prepared.as_ref().map_err(Clone::clone))
        .transpose()?
        .and_then(Option::as_ref)
    else {
        return Ok(false);
    };
    if prepared.git_tree.head != *candidate_head
        || prepared.git_tree.work_id != target.task_id
        || finding.repository_id.as_ref() != Some(&prepared.git_tree.repository_id)
        || tx.git().git_worktree(&prepared.git_tree.id)?.as_ref() != Some(&prepared.git_tree)
        || tx
            .projects()
            .repository(&prepared.git_tree.repository_id)?
            .is_none_or(|repo| repo.revision != prepared.repository_revision)
        || tx
            .artifacts()
            .artifact(&prepared.evidence.artifact_id)?
            .is_none_or(|artifact| {
                (artifact.owner.as_ref() != Some(ctx.principal())
                    && (artifact.origin_work_id.as_ref() != Some(&finding.root_id)
                        || artifact.provenance["producedBy"].as_str()
                            != Some(ctx.principal().0.as_str())))
                    || artifact.scope.project() != Some(&finding.project_id)
            })
    {
        return Ok(false);
    }
    let task = RootId(target.task_id.clone());
    for id in focused_check_ids {
        let checks = tx.operations().operation(&OperationId(id.clone()))?;
        if !checks.is_some_and(|check| {
            check.capability == "process.execute"
                && check.state == OperationState::ObservedComplete
                && (check.root_id.as_ref() == Some(&finding.root_id)
                    || check.root_id.as_ref() == Some(&task))
                && check.latest_attempt().is_some_and(|attempt| {
                    attempt.observations.iter().any(|entry| {
                        let subject = &entry["verifiedWorktree"];
                        entry["result"]["value"]["exitCode"].as_i64() == Some(0)
                            && subject["worktreeId"].as_str() == Some(prepared.git_tree.id.as_str())
                            && subject["repositoryId"].as_str()
                                == Some(prepared.git_tree.repository_id.as_str())
                            && subject["workId"].as_str() == Some(target.task_id.as_str())
                            && subject["head"] == serde_json::json!(candidate_head)
                            && subject["worktreeRevision"].as_i64()
                                == Some(prepared.git_tree.revision)
                    })
                })
        }) {
            return Ok(false);
        }
    }
    Ok(true)
}
