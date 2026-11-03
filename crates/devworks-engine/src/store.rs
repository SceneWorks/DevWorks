//! Async access to the injected store and the engine's only calls into the durable queue's
//! claim/failure/recovery primitives. Keeping them here keeps their signatures and semantics in
//! one place (claims are boot-bound; `fail(Retry)` may hold a possibly-dispatched delivery;
//! startup recovery reclaims only prior-boot claims).
use devworks_core::ports::storage::{
    ApplicationStore, ClaimRecovery, ClaimRequest, FailureDisposition, QueueEntry, QueueFence,
    ReadOptions, ReadSnapshot, RecoveryOutcome, StorageError, Unavailable, WriteOptions,
    WriteTransaction, read_snapshot, write_job,
};
use devworks_core::protocol::{Id, OperationId, OperationState};
use std::sync::Arc;

/// Runs `work` on the write owner and awaits its outcome without blocking the runtime. A lost
/// reply after admission is `CommitUnknown`, never a rollback.
pub(crate) async fn write<T, F>(
    store: &Arc<dyn ApplicationStore>,
    work: F,
) -> Result<T, StorageError>
where
    T: Send + 'static,
    F: FnOnce(&mut dyn WriteTransaction) -> Result<T, StorageError> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    store.submit_write(
        WriteOptions::default(),
        write_job(work, move |r| {
            let _ = tx.send(r);
        }),
    )?;
    rx.await.unwrap_or(Err(StorageError::CommitUnknown))
}

/// Runs a bounded read snapshot on the blocking pool (read connections are synchronous).
pub(crate) async fn read<T, F>(
    store: &Arc<dyn ApplicationStore>,
    work: F,
) -> Result<T, StorageError>
where
    T: Send + 'static,
    F: FnOnce(&dyn ReadSnapshot) -> Result<T, StorageError> + Send + 'static,
{
    let store = store.clone();
    tokio::task::spawn_blocking(move || read_snapshot(&*store, &ReadOptions::default(), work))
        .await
        .unwrap_or_else(|_| {
            Err(StorageError::unavailable(
                Unavailable::HandlerPanicked,
                "read task failed",
            ))
        })
}

// ---------------------------------------------------------------------------------------------
// Queue primitives used by the engine

pub(crate) fn claim(
    tx: &mut dyn WriteTransaction,
    consumer: &str,
    owner: &Id,
    lease_ms: i64,
    limit: u16,
) -> Result<Vec<QueueEntry>, StorageError> {
    tx.queue().claim_ready(&ClaimRequest {
        consumer: consumer.to_owned(),
        owner: owner.clone(),
        lease_ms,
        limit,
    })
}

/// Transient failure with durable backoff. The store holds the delivery in `Reconciling`
/// instead when its operation may already have dispatched, and fails it terminally once the
/// attempt budget is used up.
pub(crate) fn fail_retry(
    tx: &mut dyn WriteTransaction,
    entry: &QueueEntry,
    fence: &QueueFence,
    cause: &str,
    delay_ms: i64,
    max_attempts: i64,
) -> Result<QueueEntry, StorageError> {
    tx.queue().fail(
        &entry.id,
        fence,
        &bounded(cause),
        FailureDisposition::Retry {
            delay_ms,
            max_attempts,
        },
    )
}

/// Deterministic (poison) failure: visible terminal `Failed`.
pub(crate) fn fail_terminal(
    tx: &mut dyn WriteTransaction,
    entry: &QueueEntry,
    fence: &QueueFence,
    cause: &str,
) -> Result<QueueEntry, StorageError> {
    tx.queue().fail(
        &entry.id,
        fence,
        &bounded(cause),
        FailureDisposition::Terminal,
    )
}

pub(crate) fn hold(
    tx: &mut dyn WriteTransaction,
    entry: &QueueEntry,
    fence: &QueueFence,
    cause: &str,
) -> Result<QueueEntry, StorageError> {
    tx.queue().hold(&entry.id, fence, &bounded(cause))
}

/// Boot recovery: claims taken by earlier boots belonged to a dead process.
pub(crate) fn recover_prior_boots(
    tx: &mut dyn WriteTransaction,
    limit: u16,
) -> Result<RecoveryOutcome, StorageError> {
    tx.queue().recover_claims(ClaimRecovery::PriorBoots, limit)
}

/// Same-boot liveness recovery of claims whose lease expired.
pub(crate) fn recover_expired(
    tx: &mut dyn WriteTransaction,
    limit: u16,
) -> Result<RecoveryOutcome, StorageError> {
    tx.queue().recover_claims(ClaimRecovery::Expired, limit)
}

pub(crate) fn classify_interrupted(
    tx: &mut dyn WriteTransaction,
    limit: u16,
) -> Result<Vec<(OperationId, OperationState)>, StorageError> {
    tx.operations().classify_interrupted(limit)
}

/// Failure causes are bounded (1..=1024 bytes) on a character boundary.
pub(crate) fn bounded(cause: &str) -> String {
    let cause = if cause.is_empty() {
        "unspecified"
    } else {
        cause
    };
    let mut end = cause.len().min(1024);
    while !cause.is_char_boundary(end) {
        end -= 1;
    }
    cause[..end].to_owned()
}
