//! Review decisions belong to the exact finding and review that need the user's input.
use super::ReviewStateRecord;
use crate::context::InvocationContext;
use crate::operations::reviews::AdjudicateInput;
use crate::ports::storage::{
    ConfigScope, StorageError, StoredApproval, StoredCodeFinding, WriteTransaction,
};
use crate::protocol::records::{CodeReviewResolution, NotificationLink, ReviewFinding};
use crate::protocol::support::BoundedText;
use crate::protocol::{DomainErrorCode, Id};
use crate::services::kernel::Failure;
use crate::services::notifications::{APPROVAL, NotificationDraft, publish};
use serde_json::json;

pub(crate) const OPERATION: &str = "reviews.adjudicate";

pub(super) fn stage(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    id: &Id,
    row: &StoredCodeFinding,
    saved: &ReviewStateRecord,
    input: &AdjudicateInput,
) -> Result<(), Failure> {
    if !input.actionable {
        return Err(super::commands::refused(
            DomainErrorCode::InvalidArgument,
            "review_decision",
            id,
        ));
    }
    let next = row
        .revision
        .checked_add(1)
        .ok_or_else(|| StorageError::InvalidRecord("review decision finding revision".into()))?;
    let binding = json!({
        "rootId":row.root_id, "reviewId":row.review_id, "headDigest":saved.value.head_digest,
        "findingId":row.id, "findingRevision":next, "question":input.rationale,
        "finding":row.body, "evidence":input.evidence,
    });
    let digest = crate::fingerprint::canonical_digest(&binding)
        .map_err(|_| StorageError::InvalidRecord("review decision content".into()))?;
    if tx.decisions().approval(id)?.is_some() {
        return Err(super::commands::refused(
            DomainErrorCode::Conflict,
            "review_decision",
            id,
        ));
    }
    let proposal = StoredApproval {
        id: id.clone(),
        scope: ConfigScope::Project(row.project_id.clone()),
        revision: 1,
        state: "Pending".into(),
        operation_kind: OPERATION.into(),
        target_id: row.id.clone(),
        target_revision: next,
        content_digest: digest,
        scope_digest: crate::services::approvals::scope_digest(&binding, None),
        summary: input.rationale.0.clone(),
        artifact: input.evidence.0.first().map(|artifact| json!(artifact)),
        payload: json!({"reviewed":binding}),
        author: ctx.principal().clone(),
        author_kind: ctx.principal_kind(),
        created_at_ms: tx.now_ms(),
        decided_by: None,
        decided_at_ms: None,
        rationale: None,
        restore_epoch: tx.store().restore_epoch,
    };
    tx.decisions().put_approval(&proposal, None)?;
    publish(
        tx,
        &NotificationDraft {
            source: format!(
                "approval-{}-{}",
                crate::ports::storage::ApprovalSource::Generic.as_str(),
                id.as_str()
            ),
            category: APPROVAL.into(),
            decision_id: Some(id.clone()),
            project_id: Some(row.project_id.clone()),
            root_id: Some(row.root_id.clone()),
            title: "Code review needs a decision".into(),
            body: input.rationale.0.clone(),
            links: vec![NotificationLink {
                kind: BoundedText("Approval".into()),
                target_id: Some(id.clone()),
                url: None,
                artifact: None,
                project_id: Some(row.project_id.clone()),
            }],
            requires_input: true,
            priority: 1,
        },
    )?;
    Ok(())
}

pub(crate) fn validate(
    tx: &mut dyn WriteTransaction,
    proposal: &StoredApproval,
) -> Result<(), Failure> {
    let invalid = || {
        super::commands::refused(
            DomainErrorCode::SourceChanged,
            "review_decision",
            &proposal.id,
        )
    };
    let row = tx
        .git()
        .code_finding(&proposal.target_id)?
        .ok_or_else(invalid)?;
    let review = tx.git().code_review(&row.review_id)?.ok_or_else(invalid)?;
    let saved: ReviewStateRecord = super::decode(&review.body)?;
    let finding: ReviewFinding = super::decode(&row.body)?;
    let binding = &proposal.payload["reviewed"];
    if row.revision != proposal.target_revision
        || proposal.scope != ConfigScope::Project(row.project_id.clone())
        || binding["rootId"] != json!(row.root_id)
        || binding["reviewId"] != json!(row.review_id)
        || binding["headDigest"] != json!(saved.value.head_digest)
        || !matches!(finding.resolution, Some(CodeReviewResolution::NeedsHuman { decision_id }) if decision_id == proposal.id)
        || crate::fingerprint::canonical_digest(binding).ok().as_ref()
            != Some(&proposal.content_digest)
    {
        return Err(invalid());
    }
    Ok(())
}
