//! What an effect worker re-checks, inside its own write, before it publishes for a journaled
//! job: the job still belongs to this store and restore epoch, its authority fences hold, its
//! actor and execution roots are unchanged, the operation attempt is still the one in flight,
//! and the queue delivery still belongs to it. Each worker adds only what is specific to its
//! source (a connection, a convention, a help generation) after these.
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::AuthorityFence;
use crate::ports::storage::{OperationRecord, StorageError, WriteTransaction};
use crate::protocol::{ActorId, Id, OperationId, OperationState, RootId, StoreId};
use crate::services::{approvals, conventions::control};
use serde_json::Value;

/// The captured identity of a job: the fields every effect job carries.
pub(crate) struct JobBinding<'a> {
    pub store_id: &'a StoreId,
    pub restore_epoch: i64,
    pub fences: &'a [AuthorityFence],
    pub actor: &'a ActorId,
    pub actor_kind: &'a str,
    pub source_context: &'a Value,
    pub control_roots: &'a [(RootId, i64)],
    pub assignment_generation: Option<i64>,
}

/// Store, epoch, authority fences, actor kind, source context and execution roots, all still as
/// captured. Returns the actor's kind and the restored invocation context.
pub(crate) fn restore(
    tx: &mut dyn WriteTransaction,
    job: &JobBinding<'_>,
    operation: &OperationId,
) -> Result<(PrincipalKind, InvocationContext), StorageError> {
    if tx.store().store_id != *job.store_id
        || tx.store().restore_epoch != job.restore_epoch
        || !crate::services::fences::fences_hold(tx, job.fences)?
    {
        return Err(StorageError::ScopeDenied);
    }
    let kind = tx
        .actors()
        .actor_kind(job.actor)?
        .ok_or(StorageError::ScopeDenied)?;
    if kind.as_str() != job.actor_kind {
        return Err(StorageError::ScopeDenied);
    }
    let context =
        approvals::restore_source_context(tx.store(), job.actor, kind, job.source_context)?;
    control::check(
        tx,
        &context,
        operation,
        job.control_roots,
        job.assignment_generation,
    )?;
    Ok((kind, context))
}

/// The journaled operation still carries the job's payload, its latest attempt is the one the
/// worker holds, and it is still dispatching. `lease` names the effect in the lost-lease error.
pub(crate) fn attempt_holds(
    tx: &mut dyn WriteTransaction,
    held: &OperationRecord,
    lease: &'static str,
) -> Result<(), StorageError> {
    let actual = tx
        .operations()
        .operation(&held.id)?
        .ok_or_else(|| StorageError::InvalidRecord(format!("{lease} journal missing")))?;
    if actual.payload != held.payload
        || actual.latest_attempt().map(|a| a.fence()) != held.latest_attempt().map(|a| a.fence())
        || !matches!(
            actual.state,
            OperationState::Dispatching | OperationState::OutcomeUnknown
        )
    {
        return Err(StorageError::LeaseLost {
            kind: lease,
            id: held.id.0.as_str().into(),
        });
    }
    Ok(())
}

/// The queue delivery exists, belongs to `consumer` and `operation`, and has not been asked to
/// cancel.
pub(crate) fn delivery_holds(
    tx: &mut dyn WriteTransaction,
    delivery: &Id,
    consumer: &str,
    operation: &OperationId,
) -> Result<(), StorageError> {
    let entry = tx
        .queue()
        .entry(delivery)?
        .ok_or_else(|| StorageError::InvalidRecord(format!("{consumer} delivery missing")))?;
    if entry.consumer != consumer || entry.operation_id.as_ref() != Some(operation) {
        return Err(StorageError::ScopeDenied);
    }
    if entry.cancel_requested {
        return Err(StorageError::Cancelled);
    }
    Ok(())
}
