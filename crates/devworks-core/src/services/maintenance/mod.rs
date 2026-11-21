//! Operator previews and authorized job firings share one exact, journaled retention service.
mod scheduled;
mod worker;

use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, maintenance as ops};
use crate::ports::storage::{
    ApplicationStore, ReadSnapshot, RetentionHold, RetentionKind, RetentionPhase, RetentionWindow,
    ScheduledRetention, StorageError, StoredRetention, WriteTransaction,
};
use crate::ports::{CommandHandler, Decided, HandlerFailure, IdKind, QueryAnswer, QueryHandler};
use crate::protocol::records::{
    PruneHeldRecord, PruneHold, PruneInspection, PruneOutcome, PrunePhase, PrunePreview,
    RevisionedRef,
};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::BoundedList;
use crate::protocol::{
    ActorId, CorrelationId, DomainErrorCode, DomainFault, Id, RevisionDimension,
};
use crate::services::kernel::{Failure, Refusal, counter, decided, label, not_found, rev};
use std::sync::Arc;

pub(crate) use scheduled::{cancel_scheduled, validate_schedule};
pub use worker::EFFECT_CONSUMER;
pub(crate) use worker::QueueHook;

pub(crate) const SCHEDULED_OPERATION: &str = "maintenance.scheduled_prune";
pub const OPERATIONS: [&str; 7] = [
    "maintenance.prune_diagnostics",
    "maintenance.prune_index_runs",
    "maintenance.prune_messages",
    "maintenance.prune_sessions",
    "maintenance.inspect",
    "maintenance.cancel",
    SCHEDULED_OPERATION,
];

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
const PREVIEW_TTL_MS: i64 = 15 * 60 * 1_000;

pub struct MaintenanceService {
    store: Arc<dyn ApplicationStore>,
}

impl MaintenanceService {
    pub fn new(store: Arc<dyn ApplicationStore>) -> Self {
        Self { store }
    }
}

struct Request<'a> {
    kind: RetentionKind,
    preview_only: bool,
    preview_id: Option<&'a Id>,
    expected_revision: Option<i64>,
    cutoff: i64,
    limit: u16,
}

impl<'a> Request<'a> {
    fn from_input(input: &'a OperationInput) -> Option<Self> {
        macro_rules! request {
            ($value:ident, $kind:ident) => {
                Some(Self {
                    kind: RetentionKind::$kind,
                    preview_only: $value.preview_only,
                    preview_id: $value.preview_id.as_ref(),
                    expected_revision: $value.expected_revision.as_ref().map(|r| r.get()),
                    cutoff: $value.older_than_ms.get(),
                    limit: $value.max_items.get() as u16,
                })
            };
        }
        match input {
            OperationInput::Maintenance(ops::Input::PruneDiagnostics(v)) => {
                request!(v, Diagnostics)
            }
            OperationInput::Maintenance(ops::Input::PruneIndexRuns(v)) => request!(v, IndexRuns),
            OperationInput::Maintenance(ops::Input::PruneMessages(v)) => request!(v, Messages),
            OperationInput::Maintenance(ops::Input::PruneSessions(v)) => request!(v, Sessions),
            _ => None,
        }
    }
}

fn refuse(code: DomainErrorCode, id: Option<Id>) -> Failure {
    Refusal::new(code, "retention-preview", id).into()
}

fn human(ctx: &InvocationContext) -> Result<(), Failure> {
    if ctx.principal_kind() != PrincipalKind::Human
        || ctx.project_id().is_some()
        || ctx.root_id().is_some()
        || ctx.assignment_id().is_some()
    {
        return Err(refuse(DomainErrorCode::PermissionDenied, None));
    }
    Ok(())
}

fn record_preview(
    tx: &mut dyn WriteTransaction,
    owner: ActorId,
    kind: RetentionKind,
    cutoff: i64,
    limit: u16,
    scheduled: Option<ScheduledRetention>,
) -> Result<StoredRetention, StorageError> {
    let now = tx.now_ms().max(0);
    let effective_cutoff_ms = match kind {
        RetentionKind::Messages | RetentionKind::Sessions => {
            cutoff.min(now.saturating_sub(7 * DAY_MS).max(0))
        }
        _ => cutoff.min(now),
    };
    let window = RetentionWindow {
        kind,
        requested_cutoff_ms: cutoff,
        effective_cutoff_ms,
        captured_at_ms: now,
        limit,
    };
    let selection = tx.retention()?.preview_retention(&window)?;
    let saved = StoredRetention {
        id: tx.new_id(IdKind::Operation),
        revision: 1,
        owner,
        store: tx.store().store_id.clone(),
        restore_epoch: tx.store().restore_epoch,
        window,
        expires_at_ms: now.saturating_add(PREVIEW_TTL_MS),
        selection,
        scheduled,
        operation: None,
        phase: RetentionPhase::Previewed,
        outcome: None,
    };
    tx.retention()?.put_retention_preview(&saved, None)?;
    Ok(saved)
}

fn manual(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    command: &AdmittedCommand,
    request: Request<'_>,
) -> Result<DomainDecision, Failure> {
    human(ctx)?;
    let (id, expected) = match (
        request.preview_only,
        request.preview_id,
        request.expected_revision,
    ) {
        (true, None, None) => {
            if request.cutoff > tx.now_ms() {
                return Err(refuse(DomainErrorCode::InvalidArgument, None));
            }
            let preview = record_preview(
                tx,
                ctx.principal().clone(),
                request.kind,
                request.cutoff,
                request.limit,
                None,
            )?;
            return Ok(DomainDecision::Applied {
                result: legacy_result(&preview),
            });
        }
        (false, Some(id), Some(expected)) => (id, expected),
        _ => return Err(refuse(DomainErrorCode::InvalidArgument, None)),
    };
    let preview = tx
        .retention()?
        .retention_preview(id)?
        .filter(|preview| preview.owner == *ctx.principal() && preview.scheduled.is_none())
        .ok_or_else(|| not_found("retention-preview", id))?;
    if preview.revision != expected {
        return Err(StorageError::conflict(
            RevisionDimension::Reference,
            "retention-preview",
            id.clone(),
            rev(expected),
            rev(preview.revision),
        )
        .into());
    }
    if preview.phase != RetentionPhase::Previewed
        || preview.window.kind != request.kind
        || preview.expires_at_ms <= tx.now_ms()
        || preview.store != tx.store().store_id
        || preview.restore_epoch != tx.store().restore_epoch
        || preview.window.requested_cutoff_ms != request.cutoff
        || preview.window.limit != request.limit
    {
        return Err(refuse(DomainErrorCode::SourceChanged, Some(id.clone())));
    }
    let ids: Vec<_> = preview
        .selection
        .candidates
        .iter()
        .map(|witness| witness.id.clone())
        .collect();
    if tx.retention()?.retention_witnesses(&preview.window, &ids)? != preview.selection.candidates {
        return Err(refuse(DomainErrorCode::SourceChanged, Some(id.clone())));
    }
    worker::enqueue(tx, ctx, command, preview)
}

fn cancel(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    input: &ops::CancelInput,
) -> Result<DomainDecision, Failure> {
    human(ctx)?;
    let saved = tx
        .retention()?
        .retention_preview(&input.preview_id)?
        .filter(|saved| {
            saved.owner == *ctx.principal()
                && saved.scheduled.is_none()
                && saved.store == tx.store().store_id
                && saved.restore_epoch == tx.store().restore_epoch
        })
        .ok_or_else(|| not_found("retention-preview", &input.preview_id))?;
    if saved.revision != input.expected_revision.get() && saved.outcome.is_none() {
        return Err(StorageError::conflict(
            RevisionDimension::Reference,
            "retention-preview",
            saved.id,
            input.expected_revision.clone(),
            rev(saved.revision),
        )
        .into());
    }
    let Some(operation) = &saved.operation else {
        return Err(refuse(DomainErrorCode::InvalidArgument, Some(saved.id)));
    };
    if saved.phase != RetentionPhase::Queued {
        return Ok(DomainDecision::NoChange {
            result: OperationResult::MaintenanceCancel {
                value: inspection(&saved),
            },
        });
    }
    worker::cancel(tx, operation)?;
    let after = tx
        .retention()?
        .retention_preview(&input.preview_id)?
        .ok_or(StorageError::ScopeDenied)?;
    Ok(DomainDecision::Applied {
        result: OperationResult::MaintenanceCancel {
            value: inspection(&after),
        },
    })
}

impl CommandHandler for MaintenanceService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let outcome = match command.input() {
            OperationInput::Maintenance(ops::Input::ScheduledPrune(input)) => {
                scheduled::fire(tx, ctx, command, input)
            }
            OperationInput::Maintenance(ops::Input::Cancel(input)) => cancel(tx, ctx, input),
            input => match Request::from_input(input) {
                Some(request) => manual(tx, ctx, command, request),
                None => Err(refuse(DomainErrorCode::InvalidArgument, None)),
            },
        };
        let operation = match &outcome {
            Ok(DomainDecision::Accepted { operation_id }) => Some(operation_id.clone()),
            _ => None,
        };
        let mut decision = decided(tx, outcome)?;
        decision.operation_id = operation;
        Ok(decision)
    }
}

impl QueryHandler for MaintenanceService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        read: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let answer = (|| -> Result<_, Failure> {
            human(ctx)?;
            let OperationInput::Maintenance(ops::Input::Inspect(input)) = query.input() else {
                return Err(refuse(DomainErrorCode::InvalidArgument, None));
            };
            let preview = read
                .retention()?
                .retention_preview(&input.preview_id)?
                .filter(|preview| {
                    preview.owner == *ctx.principal()
                        && preview.store == read.stamp().store_id
                        && preview.restore_epoch == read.stamp().restore_epoch
                })
                .ok_or_else(|| not_found("retention-preview", &input.preview_id))?;
            Ok(QueryAnswer {
                result: OperationResult::MaintenanceInspect {
                    value: inspection(&preview),
                },
                stamp: read
                    .stamp()
                    .read_stamp(ctx.policy_revision().clone(), rev(1)),
            })
        })();
        answer.map_err(|error| {
            error.query_fault(
                "retention-preview",
                CorrelationId(query.query_id().0.clone()),
            )
        })
    }
}

fn view(saved: &StoredRetention) -> PrunePreview {
    PrunePreview {
        id: saved.id.clone(),
        revision: rev(saved.revision),
        kind: saved.window.kind.wire(),
        candidate_ids: BoundedList(
            saved
                .selection
                .candidates
                .iter()
                .map(|w| RevisionedRef {
                    id: w.id.clone(),
                    revision: rev(w.revision),
                })
                .collect(),
        ),
        held_ids: BoundedList(saved.selection.held.iter().map(|w| w.id.clone()).collect()),
        expires_at_ms: counter(saved.expires_at_ms),
    }
}

fn legacy_result(saved: &StoredRetention) -> OperationResult {
    let value = view(saved);
    match saved.window.kind {
        RetentionKind::Diagnostics => OperationResult::MaintenancePruneDiagnostics { value },
        RetentionKind::IndexRuns => OperationResult::MaintenancePruneIndexRuns { value },
        RetentionKind::Messages => OperationResult::MaintenancePruneMessages { value },
        RetentionKind::Sessions => OperationResult::MaintenancePruneSessions { value },
    }
}

fn inspection(saved: &StoredRetention) -> PruneInspection {
    PruneInspection {
        preview: view(saved),
        requested_cutoff_ms: counter(saved.window.requested_cutoff_ms),
        effective_cutoff_ms: counter(saved.window.effective_cutoff_ms),
        captured_at_ms: counter(saved.window.captured_at_ms),
        max_items: crate::protocol::support::Limit::new(saved.window.limit.into())
            .expect("stored limit"),
        held: BoundedList(
            saved
                .selection
                .held
                .iter()
                .map(|w| PruneHeldRecord {
                    id: w.id.clone(),
                    reason: match w.hold.unwrap_or(RetentionHold::UnclassifiedReference) {
                        RetentionHold::AuthoredHistory => PruneHold::AuthoredHistory,
                        RetentionHold::PublishedEvidence => PruneHold::PublishedEvidence,
                        RetentionHold::LiveOperation => PruneHold::LiveOperation,
                        RetentionHold::CurrentWitness => PruneHold::CurrentWitness,
                        RetentionHold::UnresolvedFailure => PruneHold::UnresolvedFailure,
                        RetentionHold::OpenConversation => PruneHold::OpenConversation,
                        RetentionHold::PendingDelivery => PruneHold::PendingDelivery,
                        RetentionHold::HandoverWindow => PruneHold::HandoverWindow,
                        RetentionHold::ReplayHead => PruneHold::ReplayHead,
                        RetentionHold::Referenced => PruneHold::Referenced,
                        RetentionHold::UnclassifiedReference => PruneHold::UnclassifiedReference,
                    },
                })
                .collect(),
        ),
        more_candidates: saved.selection.more_candidates,
        more_held: saved.selection.more_held,
        phase: match saved.phase {
            RetentionPhase::Previewed => PrunePhase::Previewed,
            RetentionPhase::Queued => PrunePhase::Queued,
            RetentionPhase::Applied => PrunePhase::Applied,
            RetentionPhase::Cancelled => PrunePhase::Cancelled,
            RetentionPhase::Refused => PrunePhase::Refused,
        },
        operation_id: saved.operation.clone(),
        outcome: saved.outcome.as_ref().map(|outcome| PruneOutcome {
            records_removed: counter(outcome.records_removed),
            owned_rows_removed: counter(outcome.owned_rows_removed),
            settled_at_ms: counter(outcome.settled_at_ms),
            reason: outcome
                .reason
                .as_ref()
                .map(|reason| label(reason, "Retention did not apply")),
        }),
    }
}
