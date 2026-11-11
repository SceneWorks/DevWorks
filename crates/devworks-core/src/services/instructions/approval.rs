//! The business owner of saved `skills.rollback` / `roles.rollback` approvals: the reviewed
//! content of an agent proposal and the exact binding the replayed activation must match.
use super::{InstructionService, event_in, invalid, target_kind};
use crate::admission::DecodedRequest;
use crate::context::InvocationContext;
use crate::operations::{OperationInput, roles as role_ops, skills as skill_ops};
use crate::ports::Preparation;
use crate::ports::storage::{
    InstructionKind, InstructionRead, InstructionRevisionState, StoredInstructionItem,
    StoredInstructionRevision, WriteTransaction,
};
use crate::protocol::{DomainErrorCode, Id};
use crate::services::approvals::OwnerPreview;
use crate::services::sessions::{Failure, Refusal};
use serde_json::{Value, json};

/// The reviewed contents and binding of one proposed revision.
pub(crate) fn preview(
    item: &StoredInstructionItem,
    target: &StoredInstructionRevision,
    proposal_id: &Id,
    expected: i64,
) -> OwnerPreview {
    let kind = target_kind(item.kind);
    OwnerPreview {
        // A confined draft is reviewed in its own project, where alone it is visible.
        scope: target.confined_project.clone().map_or_else(
            || item.scope.clone(),
            crate::ports::storage::ConfigScope::Project,
        ),
        target_id: item.id.clone(),
        target_revision: target.revision,
        summary: format!(
            "Review {kind} {} revision {} before it can become active",
            item.name, target.revision
        ),
        reviewed: json!({
            "artifact": target.content,
            "instruction": {
                "id": item.id.as_str(), "kind": item.kind.as_str(), "name": item.name,
                "origin": item.origin.as_str(), "sourcePath": item.source_path,
                "activeRevision": item.active_revision,
            },
            "revision": target.revision,
            "contentDigest": target.content_digest,
            "resources": target.resources,
            "origin": target.origin,
            "author": target.author.0.as_str(),
            "effect": "Activation only: a skill or role instruction grants no tool, permission or scope.",
        }),
        binding: binding(item, target, proposal_id, expected),
    }
}
fn binding(
    item: &StoredInstructionItem,
    target: &StoredInstructionRevision,
    proposal_id: &Id,
    expected: i64,
) -> Value {
    json!({
        "source": "Instruction",
        "instructionId": item.id.as_str(),
        "kind": item.kind.as_str(),
        "targetRevision": target.revision,
        "expectedRevision": expected,
        "contentDigest": target.content_digest,
        "proposalId": proposal_id.as_str(),
    })
}
/// Whether a stored approval binding names exactly this item, revision, content and proposal.
pub(crate) fn binding_matches(
    stored: &Value,
    item: &StoredInstructionItem,
    target: &StoredInstructionRevision,
    proposal: &Id,
) -> bool {
    stored["source"] == "Instruction"
        && stored["instructionId"] == item.id.as_str()
        && stored["kind"] == item.kind.as_str()
        && stored["targetRevision"] == target.revision
        && stored["contentDigest"] == target.content_digest.as_str()
        && stored["proposalId"] == proposal.as_str()
}

struct Request<'a> {
    kind: InstructionKind,
    id: &'a Id,
    expected: i64,
    target: i64,
    proposal: &'a Id,
}
fn request(request: &DecodedRequest) -> Result<Request<'_>, Failure> {
    let (kind, id, expected, target, proposal) = match &request.input {
        OperationInput::Skills(skill_ops::Input::Rollback(i)) => (
            InstructionKind::Skill,
            &i.instruction_id,
            i.expected_revision.get(),
            i.target_revision.get(),
            i.proposal_id.as_ref(),
        ),
        OperationInput::Roles(role_ops::Input::Rollback(i)) => (
            InstructionKind::Role,
            &i.instruction_id,
            i.expected_revision.get(),
            i.target_revision.get(),
            i.proposal_id.as_ref(),
        ),
        _ => return Err(invalid("saved_activation")),
    };
    Ok(Request {
        kind,
        id,
        expected,
        target,
        proposal: proposal.ok_or_else(|| invalid("saved_activation"))?,
    })
}
fn current(
    read: &dyn InstructionRead,
    ctx: &InvocationContext,
    r: &Request<'_>,
) -> Result<(StoredInstructionItem, StoredInstructionRevision), Failure> {
    let item = super::visible_item(read, ctx, r.id, r.kind)?;
    let target = super::visible_revision(read, ctx, &item, r.target)?;
    Ok((item, target))
}
fn changed(id: &Id) -> Failure {
    Refusal::new(
        DomainErrorCode::SourceChanged,
        "instruction_proposal",
        Some(id.clone()),
    )
    .into()
}

impl InstructionService {
    pub fn supports_approval(&self, operation: &str) -> bool {
        matches!(operation, "skills.rollback" | "roles.rollback")
    }
    /// Owner attestation for a saved activation: the actual current target and contents.
    pub fn approval_preview(
        &self,
        ctx: &InvocationContext,
        decoded: &DecodedRequest,
        env: &Preparation<'_>,
    ) -> Result<OwnerPreview, Failure> {
        let r = request(decoded)?;
        let (item, target, latest) = env.read(|read| {
            Ok((|| {
                let (item, target) = current(read.instructions(), ctx, &r)?;
                let latest = super::visible_latest(read.instructions(), ctx, &item)?;
                Ok::<_, Failure>((item, target, latest))
            })())
        })??;
        if !matches!(
            target.state,
            InstructionRevisionState::Draft | InstructionRevisionState::Proposed
        ) || latest != r.expected
        {
            return Err(changed(&item.id));
        }
        Ok(preview(&item, &target, r.proposal, r.expected))
    }
    /// The reviewed binding still describes the current item and revision.
    pub fn validate_approval(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        decoded: &DecodedRequest,
        reviewed: &OwnerPreview,
    ) -> Result<(), Failure> {
        let r = request(decoded)?;
        let (item, target) = current(tx.instructions(), ctx, &r)?;
        if super::visible_latest(tx.instructions(), ctx, &item)? != r.expected
            || preview(&item, &target, r.proposal, r.expected).binding != reviewed.binding
        {
            return Err(changed(&item.id));
        }
        Ok(())
    }
    /// A human decision over a proposal: a rejection leaves the active revision unchanged and
    /// records the outcome on the revision. Approval itself activates nothing; the replayed
    /// activation does, under its original actor and current authority.
    pub fn decide_approval(
        &self,
        tx: &mut dyn WriteTransaction,
        human: &InvocationContext,
        decoded: &DecodedRequest,
        reviewed: &OwnerPreview,
        approve: bool,
    ) -> Result<(), Failure> {
        let r = request(decoded)?;
        let Some(item) = tx.instructions().instruction_item(r.id)? else {
            return Ok(());
        };
        let Some(target) = tx.instructions().instruction_revision(&item.id, r.target)? else {
            return Ok(());
        };
        if !binding_matches(&reviewed.binding, &item, &target, r.proposal) {
            return if approve {
                Err(changed(&item.id))
            } else {
                Ok(())
            };
        }
        let mut item = item;
        item.version += 1;
        item.updated_at_ms = tx.now_ms();
        if !approve && target.state == InstructionRevisionState::Proposed {
            tx.instructions().update_instruction_item(&item)?;
            let now = tx.now_ms();
            tx.instructions().set_instruction_revision_state(
                &item.id,
                target.revision,
                InstructionRevisionState::Proposed,
                InstructionRevisionState::Rejected,
                target.approval_id.as_ref(),
                Some(now),
            )?;
            event_in(
                tx,
                human.principal(),
                &item,
                target.confined_project.as_ref(),
                "InstructionProposalRejected",
                json!({"revision": target.revision, "approvalId": target.approval_id.as_ref().map(Id::as_str)}),
            )
            .map_err(Failure::from)?;
        } else if approve {
            tx.instructions().update_instruction_item(&item)?;
            event_in(
                tx,
                human.principal(),
                &item,
                target.confined_project.as_ref(),
                "InstructionProposalApproved",
                json!({"revision": target.revision, "approvalId": target.approval_id.as_ref().map(Id::as_str)}),
            )
            .map_err(Failure::from)?;
        }
        Ok(())
    }
}
