//! `QueueDispatcher` use cases: `queue.inspect`, `queue.consumer_status`, `queue.resolve`,
//! `queue.retry` and `queue.reconcile` (DW-003, CAP-QUEUE).
//!
//! One implementation serves every entry point: the native UI and agent tools both submit to the
//! [`crate::gateway::Gateway`] with this service registered in [`super::UseCases`], so validation,
//! authority, revision/fence checks, receipts and history are shared by construction.
//!
//! # Operator resolution rules (domain-operation-reference.md "Queue")
//!
//! Resolution cannot forge a successful domain outcome, retry an unknown effect blindly or clear
//! a pause. Each command presents the delivery's current `revision` (a stale one is a durable
//! `RevisionConflict`) and its fencing generation (`expectedFence`; a stale one is `LeaseLost`).
//!
//! | Resolution | Accepted from | Effect |
//! | --- | --- | --- |
//! | `Retry` | `Failed`, `Reconciling` | Ready now with a fresh retry budget; the failure cause stays visible until it completes. |
//! | `Reconcile` | `Reconciling` | Re-evaluates the operation journal; once the outcome is established the delivery is redelivered so its handler records it idempotently. |
//! | `AcknowledgePoison` | `Failed` | `Dismissed` (terminal); its partition continues. |
//! | `CancelBeforeDispatch` | `Ready`, `Reconciling` | `Cancelled`; an undispatched operation becomes `CancelledBeforeDispatch`. |
//!
//! While the linked operation is possibly dispatched (`Dispatching`, `OutcomeUnknown`,
//! `NeedsInput`) every resolution is refused with `OutcomeUnknown` and the delivery stays held:
//! the operation's reconciler must first record what actually happened. `queue.retry` accepts
//! only `Retry` and `queue.reconcile` only `Reconcile`; `queue.resolve` accepts all four.
//!
//! # Retained CodeGraph behavior
//!
//! * `AdminController.GetJobQueue` (`GET api/admin/jobs/queue`) maps to `queue.inspect`: it lists
//!   only work that is running, failed or queued — successful work drops out — ordered running
//!   first, then failures newest first, then queued work in drain order, with attempt counts,
//!   next-attempt times and errors. Its running/queued/failed counters are `queue.consumer_status`
//!   (`leasedCount`, `pendingCount`, `poisonCount`). Divergence: the source hid a failure once a
//!   later run of the same operation and target succeeded. A required native delivery is never
//!   skipped or acknowledged by another delivery's success (async-queue-options.md "Contract to
//!   preserve"; shared-operation-contracts.md "Queries, cursors and event delivery"), so a failed
//!   delivery stays listed until it is retried to completion or dismissed.
//! * `AdminController.DismissJob` (`POST jobs/queue/{kind}/{id}/dismiss`) maps to `queue.resolve`
//!   with `AcknowledgePoison`. As in the source, only an unresolved failure is dismissable;
//!   dismissal is durable and leaves execution ownership untouched, and an unknown delivery, one
//!   that is not failed (already dismissed, completed, queued or running) is `NotFound` — the
//!   source's `false`/404 — never a silent success. The source's `kind` path segment is replaced
//!   by the opaque delivery identity.
use crate::context::InvocationContext;
use crate::fingerprint::sha256_hex;
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::OperationInput;
use crate::operations::queue::Input;
use crate::ports::storage::{
    DeliveryFilter, DeliveryKey, NewEvent, QueueEntry, QueueState, ReadSnapshot, StorageError,
    WriteTransaction, possibly_dispatched,
};
use crate::ports::{CommandHandler, Decided, HandlerFailure, IdKind, QueryAnswer, QueryHandler};
use crate::protocol::records::{ConsumerStatus, QueueDelivery, QueueResolution};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::support::{BoundedList, BoundedText, Content, Page};
use crate::protocol::{
    CorrelationId, Counter, DomainErrorCode, DomainFault, EventId, Id, OperationId, OperationState,
    RetryAdvice, Revision, RevisionDimension,
};
use serde_json::json;

/// The queue use cases (stateless; all state is in the store).
#[derive(Clone, Copy, Debug, Default)]
pub struct QueueService;

/// The operations this service implements.
pub const OPERATIONS: [&str; 5] = [
    "queue.inspect",
    "queue.resolve",
    "queue.retry",
    "queue.consumer_status",
    "queue.reconcile",
];

/// Wire state of a delivery. The queue's own states map onto the shared operation lifecycle.
pub fn delivery_state(e: &QueueEntry, operation: Option<OperationState>) -> OperationState {
    use OperationState as O;
    match e.state {
        QueueState::Ready if e.attempt_count > 0 => O::RetryPending,
        QueueState::Ready => O::IntentRecorded,
        QueueState::Claimed => O::Dispatching,
        QueueState::Completed => O::ObservedComplete,
        QueueState::Failed | QueueState::Dismissed => O::Failed,
        QueueState::Cancelled => O::CancelledBeforeDispatch,
        // Held without an operation (for example an unknown payload schema) needs a decision;
        // held behind an operation, its outcome is not yet established.
        QueueState::Reconciling => match operation {
            None | Some(O::NeedsInput) => O::NeedsInput,
            Some(_) => O::OutcomeUnknown,
        },
    }
}

fn counter(v: i64) -> Result<Counter, StorageError> {
    Counter::new(v.max(0)).map_err(|e| StorageError::InvalidRecord(e.into()))
}
fn revision(v: i64) -> Result<Revision, StorageError> {
    Revision::new(v.max(1)).map_err(|e| StorageError::InvalidRecord(e.into()))
}
fn consumer_id(consumer: &str) -> Result<Id, StorageError> {
    Id::new(consumer).map_err(|_| {
        StorageError::InvalidRecord(format!("consumer {consumer:?} is not an opaque identifier"))
    })
}
/// A bounded label of at most 256 bytes, cut on a character boundary.
fn label(text: &str) -> Option<BoundedText<1, 256>> {
    if text.is_empty() {
        return None;
    }
    let mut end = text.len().min(256);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(BoundedText(text[..end].to_owned()))
}

/// The wire record of one delivery.
pub fn delivery_record(
    e: &QueueEntry,
    operation: Option<OperationState>,
) -> Result<QueueDelivery, StorageError> {
    Ok(QueueDelivery {
        id: e.id.clone(),
        event_id: e.event_id.clone(),
        consumer_id: consumer_id(&e.consumer)?,
        revision: revision(e.revision)?,
        state: delivery_state(e, operation),
        attempt: counter(e.attempt_count)?,
        fence: counter(e.generation)?,
        lease_until_ms: e.lease_until_ms.map(counter).transpose()?,
        error: e.last_error.as_deref().and_then(label),
    })
}

fn operation_state(
    snapshot_op: impl Fn(&OperationId) -> Result<Option<OperationState>, StorageError>,
    e: &QueueEntry,
) -> Result<Option<OperationState>, StorageError> {
    match &e.operation_id {
        Some(op) => snapshot_op(op),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------------------------
// Cursors

const CURSOR_VERSION: &str = "q1";

/// Binds a cursor to its store, restore epoch and filter so a changed predicate or a restore
/// invalidates it visibly instead of resuming a different listing.
fn cursor_binding(snapshot: &dyn ReadSnapshot, filter: &DeliveryFilter) -> String {
    let stamp = snapshot.stamp();
    let digest = sha256_hex(
        format!(
            "{}|{}|{}|{}",
            stamp.store_id.0.as_str(),
            stamp.restore_epoch,
            filter.consumer.as_deref().unwrap_or(""),
            filter.operation_id.as_ref().map_or("", |o| o.0.as_str())
        )
        .as_bytes(),
    );
    digest[..16].to_owned()
}
fn encode_cursor(key: &DeliveryKey, binding: &str) -> BoundedText<1, 4096> {
    BoundedText(format!(
        "{CURSOR_VERSION}.{}.{}.{}.{}.{binding}",
        key.rank,
        key.a,
        key.b,
        key.id.as_str()
    ))
}
enum CursorError {
    Malformed,
    Stale,
}
fn decode_cursor(raw: &str, binding: &str) -> Result<DeliveryKey, CursorError> {
    let parts: Vec<&str> = raw.split('.').collect();
    let [version, rank, a, b, id, bound] = parts.as_slice() else {
        return Err(CursorError::Malformed);
    };
    if *version != CURSOR_VERSION {
        return Err(CursorError::Malformed);
    }
    let key = DeliveryKey {
        rank: rank.parse().map_err(|_| CursorError::Malformed)?,
        a: a.parse().map_err(|_| CursorError::Malformed)?,
        b: b.parse().map_err(|_| CursorError::Malformed)?,
        id: Id::new(*id).map_err(|_| CursorError::Malformed)?,
    };
    if key.rank > 2 {
        return Err(CursorError::Malformed);
    }
    if *bound != binding {
        return Err(CursorError::Stale);
    }
    Ok(key)
}

// ---------------------------------------------------------------------------------------------
// Queries

impl QueryHandler for QueueService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        snapshot: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let correlation = CorrelationId(ctx.invocation_id().clone());
        let fault = |code, retry| super::fault(code, retry, &correlation);
        let OperationInput::Queue(input) = query.input() else {
            return Err(fault(
                DomainErrorCode::UnsupportedCapability,
                RetryAdvice::Never,
            ));
        };
        let storage = |e: StorageError| fault(DomainErrorCode::StorageUnavailable, retry_of(&e));
        let op_state = |op: &OperationId| {
            snapshot
                .operations()
                .operation(op)
                .map(|o| o.map(|o| o.state))
        };
        let result = match input {
            Input::Inspect(i) => {
                let filter = DeliveryFilter {
                    consumer: i.consumer_id.as_ref().map(|c| c.as_str().to_owned()),
                    operation_id: i.operation_id.clone(),
                };
                let binding = cursor_binding(snapshot, &filter);
                let after = match &i.page.cursor {
                    None => None,
                    Some(c) => Some(decode_cursor(&c.0, &binding).map_err(|e| match e {
                        CursorError::Malformed => {
                            fault(DomainErrorCode::InvalidArgument, RetryAdvice::Never)
                        }
                        CursorError::Stale => fault(
                            DomainErrorCode::CursorExpired,
                            RetryAdvice::RefreshAndNewRequest,
                        ),
                    })?),
                };
                let limit = u16::try_from(i.page.limit.get())
                    .unwrap_or(u16::MAX)
                    .min(500);
                let page = snapshot
                    .queue()
                    .deliveries(&filter, after.as_ref(), limit)
                    .map_err(storage)?;
                let mut items = Vec::with_capacity(page.entries.len());
                for e in &page.entries {
                    let op = operation_state(op_state, e).map_err(storage)?;
                    items.push(delivery_record(e, op).map_err(storage)?);
                }
                let next_cursor = page.next.as_ref().map(|k| encode_cursor(k, &binding));
                OperationResult::QueueInspect {
                    value: Page {
                        items: BoundedList(items),
                        has_more: next_cursor.is_some(),
                        next_cursor,
                    },
                }
            }
            Input::ConsumerStatus(i) => {
                let consumer = i.consumer_id.as_str();
                let status = snapshot.queue().status(consumer).map_err(storage)?;
                let progress = snapshot.queue().progress(consumer).map_err(storage)?;
                OperationResult::QueueConsumerStatus {
                    value: ConsumerStatus {
                        consumer_id: i.consumer_id.clone(),
                        revision: revision(progress.revision + 1).map_err(storage)?,
                        cursor: counter(progress.watermark).map_err(storage)?,
                        pending_count: counter(status.ready + status.reconciling)
                            .map_err(storage)?,
                        leased_count: counter(status.claimed).map_err(storage)?,
                        poison_count: counter(status.failed).map_err(storage)?,
                        retry_at_ms: progress
                            .next_retry_at_ms
                            .map(counter)
                            .transpose()
                            .map_err(storage)?,
                    },
                }
            }
            _ => {
                return Err(fault(
                    DomainErrorCode::UnsupportedCapability,
                    RetryAdvice::Never,
                ));
            }
        };
        Ok(QueryAnswer {
            stamp: snapshot.stamp().read_stamp(
                ctx.policy_revision().clone(),
                // Queue state is host-scoped; no project scope revision applies.
                Revision::new(1).expect("positive"),
            ),
            result,
        })
    }
}

fn retry_of(e: &StorageError) -> RetryAdvice {
    match e {
        StorageError::InvalidRecord(_) | StorageError::IncompatibleSchema(_) => RetryAdvice::Never,
        _ => RetryAdvice::SameCommand,
    }
}

// ---------------------------------------------------------------------------------------------
// Commands

struct Request<'a> {
    operation: &'static str,
    delivery_id: &'a Id,
    expected_revision: &'a Revision,
    expected_fence: &'a Counter,
    resolution: &'a QueueResolution,
    reason: &'a Content,
}

fn refused(fault: DomainFault, operation_id: Option<OperationId>) -> Decided {
    Decided {
        decision: DomainDecision::Refused { fault },
        operation_id,
    }
}
fn targeted(mut f: DomainFault, id: &Id, current: Option<i64>) -> DomainFault {
    f.target_id = Some(id.clone());
    f.target_kind = Some(BoundedText("queue_delivery".into()));
    if let Some(c) = current {
        f.revision_dimension = Some(RevisionDimension::Queue);
        f.current_revision = Revision::new(c.max(1)).ok();
    }
    f
}

impl CommandHandler for QueueService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        command: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        let request = match command.input() {
            OperationInput::Queue(Input::Resolve(i)) => Request {
                operation: "queue.resolve",
                delivery_id: &i.delivery_id,
                expected_revision: &i.expected_revision,
                expected_fence: &i.expected_fence,
                resolution: &i.resolution,
                reason: &i.reason,
            },
            OperationInput::Queue(Input::Retry(i)) => Request {
                operation: "queue.retry",
                delivery_id: &i.delivery_id,
                expected_revision: &i.expected_revision,
                expected_fence: &i.expected_fence,
                resolution: &i.resolution,
                reason: &i.reason,
            },
            OperationInput::Queue(Input::Reconcile(i)) => Request {
                operation: "queue.reconcile",
                delivery_id: &i.delivery_id,
                expected_revision: &i.expected_revision,
                expected_fence: &i.expected_fence,
                resolution: &i.resolution,
                reason: &i.reason,
            },
            _ => {
                let correlation = CorrelationId(tx.new_id(IdKind::Correlation));
                return Ok(refused(
                    super::fault(
                        DomainErrorCode::UnsupportedCapability,
                        RetryAdvice::Never,
                        &correlation,
                    ),
                    None,
                ));
            }
        };
        resolve(ctx, tx, &request)
    }
}

fn resolve(
    ctx: &InvocationContext,
    tx: &mut dyn WriteTransaction,
    r: &Request<'_>,
) -> Result<Decided, HandlerFailure> {
    let correlation = CorrelationId(tx.new_id(IdKind::Correlation));
    let fault = |code, retry| super::fault(code, retry, &correlation);
    let bound = matches!(
        (r.operation, r.resolution),
        ("queue.retry", QueueResolution::Retry)
            | ("queue.reconcile", QueueResolution::Reconcile)
            | ("queue.resolve", _)
    );
    if !bound {
        return Ok(refused(
            fault(DomainErrorCode::InvalidArgument, RetryAdvice::Never),
            None,
        ));
    }
    let id = r.delivery_id;
    let Some(entry) = tx.queue().entry(id)? else {
        return Ok(refused(
            targeted(
                fault(DomainErrorCode::NotFound, RetryAdvice::Never),
                id,
                None,
            ),
            None,
        ));
    };
    if entry.revision != r.expected_revision.get() {
        return Err(StorageError::conflict(
            RevisionDimension::Queue,
            "queue_delivery",
            id.clone(),
            r.expected_revision.clone(),
            revision(entry.revision)?,
        )
        .into());
    }
    if entry.generation != r.expected_fence.get() {
        return Ok(refused(
            targeted(
                fault(
                    DomainErrorCode::LeaseLost,
                    RetryAdvice::RefreshAndNewRequest,
                ),
                id,
                Some(entry.revision),
            ),
            None,
        ));
    }
    let operation = match &entry.operation_id {
        Some(op) => tx.operations().operation(op)?,
        None => None,
    };
    if let Some(op) = &operation
        && possibly_dispatched(op.state)
    {
        // Never forge or blindly retry an effect whose outcome is not established.
        return Ok(refused(
            targeted(
                fault(
                    DomainErrorCode::OutcomeUnknown,
                    RetryAdvice::ReconcileOperation,
                ),
                id,
                Some(entry.revision),
            ),
            Some(op.id.clone()),
        ));
    }
    let conflict = || {
        Ok(refused(
            targeted(
                fault(DomainErrorCode::Conflict, RetryAdvice::RefreshAndNewRequest),
                id,
                Some(entry.revision),
            ),
            None,
        ))
    };
    let after = match r.resolution {
        QueueResolution::Retry => {
            if !matches!(entry.state, QueueState::Failed | QueueState::Reconciling) {
                return conflict();
            }
            tx.queue().requeue(id, 0)?
        }
        QueueResolution::Reconcile => {
            if entry.state != QueueState::Reconciling {
                return conflict();
            }
            tx.queue().requeue(id, 0)?
        }
        QueueResolution::AcknowledgePoison => {
            if entry.state != QueueState::Failed {
                // Retained DismissJob behavior: nothing unresolved to dismiss is not-found.
                return Ok(refused(
                    targeted(
                        fault(DomainErrorCode::NotFound, RetryAdvice::Never),
                        id,
                        None,
                    ),
                    None,
                ));
            }
            tx.queue().dismiss(id, &r.reason.0)?
        }
        QueueResolution::CancelBeforeDispatch => {
            if !matches!(entry.state, QueueState::Ready | QueueState::Reconciling) {
                return conflict();
            }
            let after = tx.queue().cancel(id)?;
            if let Some(op) = &operation
                && matches!(
                    op.state,
                    OperationState::IntentRecorded | OperationState::RetryPending
                )
            {
                tx.operations().transition(
                    &op.id,
                    op.revision,
                    None,
                    OperationState::CancelledBeforeDispatch,
                    None,
                )?;
            }
            after
        }
    };
    let event_id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event_id,
        actor: ctx.principal().clone(),
        project_id: None,
        root_id: after.root_id.clone(),
        operation_id: after.operation_id.clone(),
        aggregate_kind: "queue_entry".into(),
        aggregate_id: after.id.as_str().to_owned(),
        aggregate_revision: after.revision,
        kind: "QueueDeliveryResolved".into(),
        payload: json!({
            "operation": r.operation,
            "resolution": format!("{:?}", r.resolution),
            "reason": r.reason.0,
            "fromState": format!("{:?}", entry.state),
            "toState": format!("{:?}", after.state),
        }),
    })?;
    let op_state = match &after.operation_id {
        Some(op) => tx.operations().operation(op)?.map(|o| o.state),
        None => None,
    };
    let value = delivery_record(&after, op_state)?;
    let result = match r.operation {
        "queue.retry" => OperationResult::QueueRetry { value },
        "queue.reconcile" => OperationResult::QueueReconcile { value },
        _ => OperationResult::QueueResolve { value },
    };
    Ok(Decided {
        decision: DomainDecision::Applied { result },
        operation_id: None,
    })
}
