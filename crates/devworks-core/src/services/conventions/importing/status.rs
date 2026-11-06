use super::{
    commands::{Job, invalid},
    *,
};
use crate::{
    context::{InvocationContext, PrincipalKind},
    ports::Decided,
    protocol::{
        OperationId, Revision,
        data::DataScope,
        records::*,
        results::{DomainDecision, OperationResult},
    },
    services::{
        conventions::{access, model},
        sessions::{Failure, counter, not_found, rev},
    },
};
fn missing(id: &OperationId) -> Failure {
    not_found("convention_import", &id.0).into()
}
fn private(ctx: &InvocationContext, job: &Job) -> bool {
    ctx.principal() == &job.actor
        || matches!(
            ctx.principal_kind(),
            PrincipalKind::Human | PrincipalKind::Host
        )
}
fn status(
    op: OperationRecord,
    d: StoredConventionImport,
    cancel: bool,
) -> ConventionOperationStatus {
    let phase = match (op.state, d.phase.as_str()) {
        (crate::protocol::OperationState::CancelledBeforeDispatch, _) => {
            ConventionOperationPhase::Cancelled
        }
        (crate::protocol::OperationState::Failed, _) => ConventionOperationPhase::Failed,
        (crate::protocol::OperationState::ObservedComplete, "Published") => {
            ConventionOperationPhase::Completed
        }
        (_, "Published") => ConventionOperationPhase::Publishing,
        (_, "Failed") => ConventionOperationPhase::Failed,
        (_, "Cancelled") => ConventionOperationPhase::Cancelled,
        (_, "Parsing") => ConventionOperationPhase::Preparing,
        (_, "Ready") => ConventionOperationPhase::Publishing,
        _ => ConventionOperationPhase::Queued,
    };
    ConventionOperationStatus {
        operation: OperationRef {
            operation_id: op.id,
            state: op.state,
        },
        revision: rev(op.revision),
        kind: ConventionOperationKind::Import,
        scope: model::scope(&d.scope),
        phase,
        processed_items: counter(d.processed_resources),
        total_items: d.total_resources.map(counter),
        convention_id: Some(d.convention_id),
        can_cancel: !matches!(d.phase.as_str(), "Published" | "Failed" | "Cancelled") && !cancel,
        cancel_requested: cancel,
        reason: d.error.map(crate::protocol::support::BoundedText),
    }
}
impl ImportService {
    pub fn status(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        id: &OperationId,
        scope: Option<&DataScope>,
    ) -> Result<ConventionOperationStatus, Failure> {
        let op = read
            .operations()
            .operation(id)?
            .filter(|o| o.capability == "conventions.import")
            .ok_or_else(|| missing(id))?;
        let job: Job = serde_json::from_value(op.payload.clone()).map_err(|_| missing(id))?;
        let d = read
            .conventions()
            .convention_import(id)?
            .ok_or_else(|| missing(id))?;
        let scoped = access::scope(read, ctx, scope, false)?;
        if !private(ctx, &job) || !scoped.sees(&d.scope) {
            return Err(missing(id));
        }
        let q = read
            .queue()
            .entry(&job.delivery)?
            .ok_or_else(|| missing(id))?;
        Ok(status(op, d, q.cancel_requested))
    }
    pub fn cancel(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        id: &OperationId,
        expected: &Revision,
        scope: Option<&DataScope>,
    ) -> Result<Decided, Failure> {
        let op = tx
            .operations()
            .operation(id)?
            .filter(|o| o.capability == "conventions.import")
            .ok_or_else(|| missing(id))?;
        let job: Job = serde_json::from_value(op.payload.clone()).map_err(|_| missing(id))?;
        let d = tx
            .conventions()
            .convention_import(id)?
            .ok_or_else(|| missing(id))?;
        let scoped = access::scope_in(tx, ctx, scope, false)?;
        if !private(ctx, &job) || !scoped.sees(&d.scope) {
            return Err(missing(id));
        }
        access::writable(tx, ctx, &d.scope)?;
        if expected.get() != op.revision {
            return Err(StorageError::conflict(
                crate::protocol::RevisionDimension::Source,
                "convention_import",
                id.0.clone(),
                expected.clone(),
                rev(op.revision),
            )
            .into());
        }
        let q = tx
            .queue()
            .entry(&job.delivery)?
            .ok_or_else(|| invalid("import queue missing"))?;
        let changed = !matches!(d.phase.as_str(), "Published" | "Failed" | "Cancelled")
            && !q.cancel_requested;
        let d = if changed {
            tx.queue().cancel(&q.id)?;
            tx.conventions().set_convention_import_phase(
                id,
                d.revision,
                "Cancelled",
                None,
                None,
                None,
            )?
        } else {
            d
        };
        let result = OperationResult::ConventionsRetire {
            value: ConventionMutationResult::Current(ConventionMutation {
                id: id.0.clone(),
                revision: rev(op.revision),
                changed,
                convention: None,
                resource: None,
                operation: Some(status(op, d, q.cancel_requested || changed)),
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
