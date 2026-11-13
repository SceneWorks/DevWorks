//! An agent's tool call that a domain command refused `ApprovalRequired`. The host stages a
//! saved-operation approval for the exact refused request (its request digest) and an operation
//! that tracks it, so the call is answered `ApprovalRequired` at once and the person's decision
//! later reaches the agent as an update of that operation. Approval security is unchanged: the
//! saved request is the refused request byte for byte, and its replay keeps the agent as actor.
use super::service::{invalid, unsupported};
use super::{ApprovalService, OwnerPreview, model};
use crate::context::InvocationContext;
use crate::ports::Preparation;
use crate::ports::storage::*;
use crate::protocol::{Id, OperationId, OperationState};
use crate::services::sessions::Failure;
use serde_json::{Value, json};
use std::sync::Arc;

/// The capability of the operation that tracks a staged approval.
pub const HELD_CAPABILITY: &str = "approvals.saved_operation";

/// How many decided approvals of one exact request an agent may have before another is refused.
const MAX_REQUESTS: u32 = 64;

/// The approval and operation that answer a refused call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldApproval {
    pub approval_id: Id,
    pub operation_id: OperationId,
}

/// The operation that tracks approval `approval`; one per approval.
pub fn operation_for(approval: &Id) -> OperationId {
    OperationId(
        Id::new(format!(
            "held-op-{}",
            &model::hash(&json!(["operation", approval]))[..40]
        ))
        .expect("stable operation id"),
    )
}

fn approval_for(ctx: &InvocationContext, operation: &str, digest: &str, n: u32) -> Id {
    Id::new(format!(
        "held-{}",
        &model::hash(&json!([
            ctx.principal(),
            ctx.restore_epoch(),
            operation,
            digest,
            n
        ]))[..40]
    ))
    .expect("stable approval id")
}

impl ApprovalService {
    /// Stages (or finds) the approval for a refused call. `digest` is the refused request's
    /// fingerprint; the saved input must decode to exactly that request. An undecided approval of
    /// the same request by the same agent is reused, so sending the call again asks once.
    pub fn stage_held(
        self: &Arc<Self>,
        store: &dyn ApplicationStore,
        ctx: &InvocationContext,
        operation: &str,
        schema_revision: u32,
        input: &Value,
        digest: &str,
    ) -> Result<HeldApproval, Failure> {
        let request = self.decoded(operation, schema_revision, input)?;
        if request.fingerprint.as_str() != digest {
            return Err(invalid("held_request_digest"));
        }
        let env = Preparation::new(store, None, None);
        if !env.read(|s| super::prepare_authority(s, ctx, &request))? {
            return Err(super::service::denied("saved_operation_current_authority"));
        }
        if super::service::builtin(operation) {
            return Err(unsupported());
        }
        let preview: OwnerPreview = self.owners.prepare(ctx, &request, &env)?;
        let content = json!({"kind": "SavedOperation",
            "schemaRevision": schema_revision.to_string(), "input": input});
        let scope = match &preview.scope {
            ConfigScope::Workspace => json!({"kind": "Workspace"}),
            ConfigScope::Project(p) => json!({"kind": "Project", "projectId": p}),
        };
        let proposal: crate::operations::approvals::ProposeInput = serde_json::from_value(json!({
            "scope": scope,
            "operationKind": operation,
            "target": {"id": preview.target_id, "revision": preview.target_revision.to_string()},
            "contentDigest": digest,
            "summary": preview.summary,
            "payload": content,
        }))
        .map_err(|_| invalid("held_proposal"))?;
        let ctx = ctx.clone();
        let operation = operation.to_owned();
        let digest = digest.to_owned();
        let service = self.clone();
        run_write(store, WriteOptions::default(), move |tx| {
            let mut stage = || -> Result<HeldApproval, Failure> {
                for n in 0..MAX_REQUESTS {
                    let id = approval_for(&ctx, &operation, &digest, n);
                    match tx.decisions().approval(&id)? {
                        None => {
                            service.validate_owner(tx, &ctx, &request, &preview)?;
                            super::writes::stage_saved_as(
                                tx,
                                &ctx,
                                &proposal,
                                &request,
                                &preview,
                                Some(id.clone()),
                            )?;
                            let operation_id = operation_for(&id);
                            tx.operations().record_intent(NewOperation {
                                id: operation_id.clone(),
                                root_id: ctx.root_id().cloned(),
                                assignment_id: ctx.assignment_id().cloned(),
                                capability: HELD_CAPABILITY.into(),
                                schema_version: i64::from(schema_revision),
                                args_digest: digest.clone(),
                                payload: json!({"approvalId": id, "operation": operation}),
                            })?;
                            return Ok(HeldApproval {
                                approval_id: id,
                                operation_id,
                            });
                        }
                        // Still awaiting the person or its readmission: the same request asks once.
                        Some(_)
                            if tx
                                .operations()
                                .operation(&operation_for(&id))?
                                .is_some_and(|o| !is_terminal(o.state)) =>
                        {
                            return Ok(HeldApproval {
                                operation_id: operation_for(&id),
                                approval_id: id,
                            });
                        }
                        Some(_) => {}
                    }
                }
                Err(invalid("held_request_repeated"))
            };
            // A storage failure rolls the unit of work back; a refusal staged nothing.
            match stage() {
                Err(Failure::Storage(error)) => Err(error),
                staged => Ok(staged),
            }
        })?
    }
}

/// Records a decision or a readmission outcome on the operation that tracks `approval`, if it has
/// one. `state` is the approval resume state (`Admitted`, `Refused`, `Unknown`) or `Rejected`.
pub(crate) fn settle(
    tx: &mut dyn WriteTransaction,
    approval: &Id,
    state: &str,
    observation: Value,
) -> Result<(), StorageError> {
    let id = operation_for(approval);
    let Some(mut op) = tx.operations().operation(&id)? else {
        return Ok(());
    };
    let to = match state {
        "Admitted" => OperationState::ObservedComplete,
        "Refused" => OperationState::Failed,
        "Unknown" => OperationState::OutcomeUnknown,
        "Rejected" => OperationState::CancelledBeforeDispatch,
        _ => return Err(StorageError::InvalidRecord("held operation state".into())),
    };
    if op.state == OperationState::IntentRecorded {
        tx.operations()
            .begin_attempt(&id, op.revision, AttemptBinding::default())?;
        op = tx
            .operations()
            .operation(&id)?
            .ok_or_else(|| StorageError::InvalidRecord("held operation".into()))?;
        if to != OperationState::CancelledBeforeDispatch {
            let fence = op.latest_attempt().map(AttemptRecord::fence);
            op = tx.operations().transition(
                &id,
                op.revision,
                fence.as_ref(),
                OperationState::Dispatching,
                None,
            )?;
        }
    }
    if op.state == to || is_terminal(op.state) {
        return Ok(());
    }
    let fence = op.latest_attempt().map(AttemptRecord::fence);
    tx.operations()
        .transition(&id, op.revision, fence.as_ref(), to, Some(observation))?;
    Ok(())
}

/// The agent context and session an approval's tracked operation reports to, with the operation
/// and its current state, if `approval` was staged for a refused call.
pub fn held_target(
    store: &dyn ApplicationStore,
    approval: &Id,
) -> Result<Option<(InvocationContext, Id, OperationId, OperationState)>, StorageError> {
    read_snapshot(store, &ReadOptions::default(), |s| {
        let id = operation_for(approval);
        let Some(op) = s.operations().operation(&id)? else {
            return Ok(None);
        };
        let Some(p) = s.decisions().approval(approval)? else {
            return Ok(None);
        };
        let ctx = super::replay::restore_context(&store.info(), s, &p, None)?;
        let Some(session) = ctx.session_id().cloned() else {
            return Ok(None);
        };
        Ok(Some((ctx, session, id, op.state)))
    })
}
