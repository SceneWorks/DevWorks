//! Scoped observation and cancellation of the actual index operation, not a History grant.
use super::{
    ConventionService, access,
    indexing::commands::{EFFECT_CONSUMER, IndexJob, IndexTarget, JOB_KIND},
    model,
};
use crate::{
    context::{InvocationContext, PrincipalKind},
    ports::{Decided, storage::*},
    protocol::{
        OperationId, OperationState, Revision,
        data::DataScope,
        records::*,
        results::{DomainDecision, OperationResult},
    },
    services::sessions::{Failure, Refusal, counter, rev},
};
fn missing(id: &OperationId) -> Failure {
    crate::services::sessions::not_found("convention_operation", &id.0).into()
}
fn private(ctx: &InvocationContext, job: &IndexJob) -> bool {
    ctx.principal() == &job.actor
        || matches!(
            ctx.principal_kind(),
            PrincipalKind::Human | PrincipalKind::Host
        )
}
fn job(op: &OperationRecord) -> Result<IndexJob, Failure> {
    if op.capability != "conventions.index" {
        return Err(missing(&op.id));
    }
    let job: IndexJob = serde_json::from_value(op.payload.clone()).map_err(|_| missing(&op.id))?;
    if job.kind != JOB_KIND {
        return Err(missing(&op.id));
    }
    Ok(job)
}
fn progress(read: &dyn ConventionIndexRead, id: &OperationId) -> Result<i64, StorageError> {
    let mut total = 0_u64;
    let mut after = None;
    loop {
        let rows = read.convention_index_generations(None, Some(id), after.as_ref(), 100)?;
        if rows.is_empty() {
            break;
        }
        after = rows.last().map(|r| r.generation_id.clone());
        for row in rows {
            total = total
                .checked_add(read.convention_chunk_count(&row.generation_id)?)
                .ok_or_else(|| StorageError::InvalidRecord("index progress overflow".into()))?;
        }
    }
    i64::try_from(total).map_err(|_| StorageError::InvalidRecord("index progress overflow".into()))
}
fn project(
    op: OperationRecord,
    job: IndexJob,
    entry: &QueueEntry,
    processed: i64,
) -> ConventionOperationStatus {
    let phase = match op.state {
        OperationState::ObservedComplete => ConventionOperationPhase::Completed,
        OperationState::Failed => ConventionOperationPhase::Failed,
        OperationState::CancelledBeforeDispatch => ConventionOperationPhase::Cancelled,
        OperationState::OutcomeUnknown | OperationState::NeedsInput => {
            ConventionOperationPhase::Unknown
        }
        OperationState::Dispatching => ConventionOperationPhase::Publishing,
        OperationState::Preflighted => ConventionOperationPhase::Preparing,
        _ => ConventionOperationPhase::Queued,
    };
    let complete = op.state == OperationState::ObservedComplete;
    ConventionOperationStatus {
        operation: OperationRef {
            operation_id: op.id,
            state: op.state,
        },
        revision: rev(op.revision),
        kind: ConventionOperationKind::Index,
        scope: job.scope,
        phase,
        processed_items: counter(processed),
        total_items: complete.then(|| counter(processed)),
        convention_id: match job.target {
            IndexTarget::One { convention_id, .. } => Some(convention_id),
            IndexTarget::All { .. } => None,
        },
        can_cancel: !is_terminal(op.state) && !entry.cancel_requested,
        cancel_requested: entry.cancel_requested,
        reason: entry
            .last_error
            .clone()
            .map(crate::protocol::support::BoundedText),
    }
}
impl ConventionService {
    pub(super) fn index_operation(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        id: &OperationId,
        scope: Option<&DataScope>,
    ) -> Result<ConventionOperationStatus, Failure> {
        let operation = read
            .operations()
            .operation(id)?
            .ok_or_else(|| missing(id))?;
        let job = job(&operation)?;
        let scoped = access::scope(read, ctx, scope, false)?;
        if !private(ctx, &job) || !scoped.sees(&model::scope_storage(&job.scope)) {
            return Err(missing(id));
        }
        let entry = read
            .queue()
            .entry(&job.delivery_id)?
            .filter(|e| e.consumer == EFFECT_CONSUMER && e.operation_id.as_ref() == Some(id))
            .ok_or_else(|| missing(id))?;
        Ok(project(
            operation,
            job,
            &entry,
            progress(read.conventions(), id)?,
        ))
    }
    pub(super) fn cancel_index(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        id: &OperationId,
        expected: &Revision,
        scope: Option<&DataScope>,
    ) -> Result<Decided, Failure> {
        let operation = tx.operations().operation(id)?.ok_or_else(|| missing(id))?;
        let job = job(&operation)?;
        let scoped = access::scope_in(tx, ctx, scope, false)?;
        if !private(ctx, &job) || !scoped.sees(&model::scope_storage(&job.scope)) {
            return Err(missing(id));
        }
        access::writable(tx, ctx, &model::scope_storage(&job.scope))?;
        if expected.get() != operation.revision {
            return Err(StorageError::conflict(
                crate::protocol::RevisionDimension::Source,
                "convention_operation",
                id.0.clone(),
                expected.clone(),
                rev(operation.revision),
            )
            .into());
        }
        let entry = tx
            .queue()
            .entry(&job.delivery_id)?
            .filter(|e| e.consumer == EFFECT_CONSUMER && e.operation_id.as_ref() == Some(id))
            .ok_or_else(|| missing(id))?;
        let changed = !is_terminal(operation.state) && !entry.cancel_requested;
        let entry = if changed {
            tx.queue().cancel(&entry.id)?
        } else {
            entry
        };
        let operation = tx.operations().operation(id)?.ok_or_else(|| missing(id))?;
        let revision = rev(operation.revision);
        let status = project(operation, job, &entry, progress(tx.conventions(), id)?);
        let result = OperationResult::ConventionsRetire {
            value: ConventionMutationResult::Current(ConventionMutation {
                id: id.0.clone(),
                revision,
                changed,
                convention: None,
                resource: None,
                operation: Some(status),
            }),
        };
        Ok(Decided {
            decision: if changed {
                DomainDecision::Applied { result }
            } else {
                DomainDecision::NoChange { result }
            },
            operation_id: None,
        })
    }
}
