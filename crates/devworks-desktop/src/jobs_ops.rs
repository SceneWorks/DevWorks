//! The requests the Scheduled jobs screen waits for. The jobs frames are in [`crate::ops`]; the
//! frames of its Deliveries subview (`queue.*`) are here.
use crate::backend::{Backend, screen_slot};
use crate::frame::{self, Negotiate};
use crate::ops::{Failure, PAGE_LIMIT, command_result, query_result, unexpected};
use crate::paged::Page;
use devworks_core::gateway::{CommandOutcome, QueryOutcome};
use devworks_core::operations::{OperationInput as In, queue};
use devworks_core::protocol::records::{QueueDelivery, QueueResolution};
use devworks_core::protocol::results::OperationResult;

/// The jobs requests. The names are persisted (see [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum JobsSlot {
    /// The jobs of the selected scope, and a further page of them.
    JobsList,
    /// A further page of the shown jobs list (that list's generation).
    JobsMore(u64),
    /// A further page re-read by a refresh of the shown jobs list (the refresh's generation).
    JobsRefresh(u64),
    /// The jobs of one project, for its run status on All projects.
    ProjectJobs(String),
    /// One job's inspection (its first run-history page) and an older history page (job ID).
    JobInspect(String),
    JobHistory(String),
    /// The job's active run, inspected by its ID (job ID): the latest occurrence may be a
    /// skipped one while the active run continues.
    JobActiveRun(String),
    /// A scoped prompt page of one form generation (None is workspace).
    JobPrompts(u64, Option<String>),
    JobCreate,
    /// A command of one job or its run (one at a time per job).
    JobCommand(String),
    /// A page of the running, failed, held and queued deliveries (`queue.inspect`; the list
    /// request's generation).
    Deliveries(u64),
    /// A resolution of one delivery (delivery ID; one at a time per delivery).
    DeliveryCommand(String),
}
screen_slot!(JobsSlot, Jobs);

/// What a person decides about one delivery. The queue owner decides whether it applies: a
/// state that does not accept it, an effect whose outcome is not established, or a receipt's
/// reply (which is never cancelled, and dismissed only when it can never be honoured) is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryResolution {
    /// `queue.retry`: ready again with a fresh retry budget.
    Retry,
    /// `queue.reconcile`: redelivered once its operation's outcome is established.
    Reconcile,
    /// `queue.resolve` with `AcknowledgePoison`: a failure that can never succeed is settled.
    Dismiss,
    /// `queue.resolve` with `CancelBeforeDispatch`: undispatched work is cancelled.
    Cancel,
}
impl DeliveryResolution {
    pub fn operation(self) -> &'static str {
        match self {
            Self::Retry => "queue.retry",
            Self::Reconcile => "queue.reconcile",
            Self::Dismiss | Self::Cancel => "queue.resolve",
        }
    }
}

/// Deliveries one page request asks for.
pub const DELIVERY_PAGE: u16 = PAGE_LIMIT;

/// `queue.inspect` of every consumer's deliveries, after `cursor`.
pub fn queue_inspect(backend: &dyn Backend, cursor: Option<&str>) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        Some(In::Queue(queue::Input::Inspect(queue::InspectInput {
            consumer_id: None,
            operation_id: None,
            page: frame::page(DELIVERY_PAGE.into(), cursor)?,
        })))
    })
}
/// A page of deliveries with the cursor of the next one (present only while more exist).
pub fn queue_page(outcome: QueryOutcome) -> Result<Page<QueueDelivery>, Failure> {
    match query_result(outcome)? {
        (OperationResult::QueueInspect { value }, _) => {
            let next = value.next_cursor.map(|c| c.0).filter(|_| value.has_more);
            if value.has_more && next.is_none() {
                return Err(Failure::Other(
                    "queue.inspect reported more pages without a cursor".into(),
                ));
            }
            Ok(Page::new(value.items.0, next))
        }
        (other, _) => Err(unexpected("queue.inspect", &other)),
    }
}
/// The resolution of `delivery` at its listed revision and fencing generation (a delivery that
/// changed meanwhile is refused, never resolved blindly).
pub fn queue_resolve(
    backend: &dyn Backend,
    delivery: &QueueDelivery,
    resolution: DeliveryResolution,
    reason: &str,
) -> Vec<u8> {
    frame::with(backend, Negotiate::Latest, || {
        let (delivery_id, expected_revision, expected_fence) = (
            delivery.id.clone(),
            delivery.revision.clone(),
            delivery.fence.clone(),
        );
        let reason = frame::text(reason)?;
        Some(In::Queue(match resolution {
            DeliveryResolution::Retry => queue::Input::Retry(queue::RetryInput {
                delivery_id,
                expected_revision,
                expected_fence,
                resolution: QueueResolution::Retry,
                reason,
            }),
            DeliveryResolution::Reconcile => queue::Input::Reconcile(queue::ReconcileInput {
                delivery_id,
                expected_revision,
                expected_fence,
                resolution: QueueResolution::Reconcile,
                reason,
            }),
            DeliveryResolution::Dismiss | DeliveryResolution::Cancel => {
                queue::Input::Resolve(queue::ResolveInput {
                    delivery_id,
                    expected_revision,
                    expected_fence,
                    resolution: if resolution == DeliveryResolution::Dismiss {
                        QueueResolution::AcknowledgePoison
                    } else {
                        QueueResolution::CancelBeforeDispatch
                    },
                    reason,
                })
            }
        }))
    })
}
/// The delivery a resolution left.
pub fn queue_resolved(outcome: CommandOutcome) -> Result<QueueDelivery, Failure> {
    match command_result(outcome)? {
        OperationResult::QueueRetry { value }
        | OperationResult::QueueReconcile { value }
        | OperationResult::QueueResolve { value } => Ok(value),
        other => Err(unexpected("queue resolution", &other)),
    }
}
