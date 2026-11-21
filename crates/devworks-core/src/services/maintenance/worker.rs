//! The deletion and its outcome are published atomically by the same write owner.
use super::{MaintenanceService, SCHEDULED_OPERATION, human, inspection, legacy_result, scheduled};
use crate::context::InvocationContext;
use crate::gateway::AdmittedCommand;
use crate::ports::storage::{
    Cancellation, NewEvent, NewOperation, NewQueueEntry, OperationRecord, QueueEntry, QueueState,
    ReadOptions, RetentionOutcome, RetentionPhase, StorageError, StoredRetention, WriteOptions,
    WriteTransaction, read_snapshot, run_write,
};
use crate::ports::{AuthorityFence, IdKind};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{ActorId, EventId, Id, OperationId, OperationState, RootId, StoreId};
use crate::services::effect::{EffectOutcome, JournaledEffect};
use crate::services::kernel::Failure;
use crate::services::{approvals, conventions::control, effect_job};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const EFFECT_CONSUMER: &str = "operational_retention";

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Job {
    preview_id: Id,
    actor: ActorId,
    actor_kind: String,
    store_id: StoreId,
    restore_epoch: i64,
    source_context: Value,
    fences: Vec<AuthorityFence>,
    delivery: Id,
    controls: Vec<(RootId, i64)>,
    assignment_generation: Option<i64>,
}

fn decode(operation: &OperationRecord) -> Result<Job, StorageError> {
    if !super::OPERATIONS.contains(&operation.capability.as_str())
        || matches!(
            operation.capability.as_str(),
            "maintenance.inspect" | "maintenance.cancel"
        )
    {
        return Err(StorageError::ScopeDenied);
    }
    serde_json::from_value(operation.payload.clone())
        .map_err(|_| StorageError::InvalidRecord("invalid operational retention effect".into()))
}

pub(super) fn enqueue(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    command: &AdmittedCommand,
    mut preview: StoredRetention,
) -> Result<DomainDecision, Failure> {
    let id = OperationId(tx.new_id(IdKind::Operation));
    let delivery = tx.new_id(IdKind::QueueEntry);
    let controls = control::capture(tx, ctx)?;
    let args = Job {
        preview_id: preview.id.clone(),
        actor: ctx.principal().clone(),
        actor_kind: ctx.principal_kind().as_str().into(),
        store_id: preview.store.clone(),
        restore_epoch: preview.restore_epoch,
        source_context: approvals::save_source_context(ctx),
        fences: command.authorization().fences().to_vec(),
        delivery: delivery.clone(),
        controls: controls.roots,
        assignment_generation: controls.assignment_generation,
    };
    tx.operations().record_intent(NewOperation {
        id: id.clone(),
        root_id: None,
        assignment_id: None,
        capability: command.descriptor().name.into(),
        schema_version: i64::from(command.schema_revision().0),
        args_digest: command.fingerprint().as_str().into(),
        payload: json!(args),
    })?;
    let expected = preview.revision;
    preview.revision += 1;
    preview.phase = RetentionPhase::Queued;
    preview.operation = Some(id.clone());
    tx.retention()?
        .put_retention_preview(&preview, Some(expected))?;
    let event_id = EventId(tx.new_id(IdKind::Event));
    tx.history().append(NewEvent {
        id: event_id.clone(),
        actor: ctx.principal().clone(),
        project_id: None,
        root_id: None,
        operation_id: Some(id.clone()),
        aggregate_kind: EFFECT_CONSUMER.into(),
        aggregate_id: preview.id.as_str().into(),
        aggregate_revision: preview.revision,
        kind: "OperationalRetentionRequested".into(),
        payload: json!({"kind":preview.window.kind,"selected":preview.selection.candidates.len()}),
    })?;
    let now = tx.now_ms();
    tx.queue().enqueue(NewQueueEntry {
        id: delivery,
        consumer: EFFECT_CONSUMER.into(),
        event_id,
        root_id: None,
        operation_id: Some(id.clone()),
        partition: None,
        priority: 0,
        ready_at_ms: now,
        payload: json!({}),
    })?;
    Ok(DomainDecision::Accepted { operation_id: id })
}

fn matches_preview(preview: &StoredRetention, operation: &OperationRecord, job: &Job) -> bool {
    preview.id == job.preview_id
        && preview.store == job.store_id
        && preview.restore_epoch == job.restore_epoch
        && preview.operation.as_ref() == Some(&operation.id)
        && match &preview.scheduled {
            Some(_) => operation.capability == SCHEDULED_OPERATION,
            None => preview.window.kind.operation() == operation.capability,
        }
}

fn obtain(
    tx: &mut dyn WriteTransaction,
    operation: &OperationRecord,
    job: &Job,
) -> Result<StoredRetention, StorageError> {
    let preview = tx
        .retention()?
        .retention_for_operation(&operation.id)?
        .filter(|saved| matches_preview(saved, operation, job))
        .ok_or(StorageError::ScopeDenied)?;
    if tx.store().store_id != job.store_id || tx.store().restore_epoch != job.restore_epoch {
        return Err(StorageError::ScopeDenied);
    }
    Ok(preview)
}

fn persist(
    tx: &mut dyn WriteTransaction,
    mut preview: StoredRetention,
    phase: RetentionPhase,
    mut outcome: RetentionOutcome,
) -> Result<StoredRetention, StorageError> {
    let expected = preview.revision;
    outcome.settled_at_ms = tx.now_ms();
    preview.revision += 1;
    preview.outcome = Some(outcome);
    preview.phase = phase;
    tx.retention()?
        .put_retention_preview(&preview, Some(expected))?;
    Ok(preview)
}

fn refused(
    tx: &mut dyn WriteTransaction,
    preview: StoredRetention,
    phase: RetentionPhase,
    reason: &str,
) -> Result<StoredRetention, StorageError> {
    persist(
        tx,
        preview,
        phase,
        RetentionOutcome {
            reason: Some(reason.to_owned()),
            ..RetentionOutcome::default()
        },
    )
}

fn qualify(
    tx: &mut dyn WriteTransaction,
    operation: &OperationRecord,
    job: &Job,
    preview: &StoredRetention,
) -> Result<(), StorageError> {
    let (_, context) = effect_job::restore(
        tx,
        &effect_job::JobBinding {
            store_id: &job.store_id,
            restore_epoch: job.restore_epoch,
            fences: &job.fences,
            actor: &job.actor,
            actor_kind: &job.actor_kind,
            source_context: &job.source_context,
            control_roots: &job.controls,
            assignment_generation: job.assignment_generation,
        },
        &operation.id,
    )?;
    if let Some(binding) = &preview.scheduled {
        let (owner, current, _) = scheduled::authorization(tx, &context, Some(&operation.id))?;
        if &current != binding
            || owner != preview.owner
            || (preview.window.kind == crate::ports::storage::RetentionKind::Diagnostics
                && preview.window.effective_cutoff_ms
                    > scheduled::diagnostics_cutoff(tx, preview.window.captured_at_ms)?)
        {
            return Err(StorageError::ScopeDenied);
        }
    } else {
        human(&context).map_err(Failure::into_storage)?;
        if context.principal() != &preview.owner {
            return Err(StorageError::ScopeDenied);
        }
    }
    Ok(())
}

fn delivery_is_bound(entry: &QueueEntry, operation: &OperationId) -> bool {
    entry.consumer == EFFECT_CONSUMER && entry.operation_id.as_ref() == Some(operation)
}

fn execute_in(
    tx: &mut dyn WriteTransaction,
    operation: &OperationRecord,
    job: &Job,
) -> Result<StoredRetention, StorageError> {
    let preview = obtain(tx, operation, job)?;
    if preview.outcome.is_some() {
        return Ok(preview);
    }
    effect_job::attempt_holds(tx, operation, EFFECT_CONSUMER)?;
    let delivery = tx
        .queue()
        .entry(&job.delivery)?
        .filter(|entry| delivery_is_bound(entry, &operation.id))
        .ok_or(StorageError::ScopeDenied)?;
    let attempt = operation
        .latest_attempt()
        .ok_or(StorageError::ScopeDenied)?;
    if operation.state != OperationState::Dispatching
        || delivery.state != QueueState::Claimed
        || Some(delivery.generation) != attempt.binding.owner_generation
        || delivery.claim_boot_id.as_ref() != tx.store().boot_id.as_ref()
        || Some(&attempt.boot_id) != tx.store().boot_id.as_ref()
        || delivery.lease_until_ms.is_none_or(|end| end <= tx.now_ms())
        || preview.phase != RetentionPhase::Queued
    {
        return Err(StorageError::LeaseLost {
            kind: EFFECT_CONSUMER,
            id: job.delivery.as_str().into(),
        });
    }
    if delivery.cancel_requested {
        return refused(
            tx,
            preview,
            RetentionPhase::Cancelled,
            "Cancelled before removing records",
        );
    }
    match qualify(tx, operation, job, &preview) {
        Ok(()) => {}
        Err(StorageError::ScopeDenied) => {
            return refused(
                tx,
                preview,
                RetentionPhase::Refused,
                "Authority or job configuration changed",
            );
        }
        Err(error) => return Err(error),
    }
    if preview.expires_at_ms <= tx.now_ms() {
        return refused(
            tx,
            preview,
            RetentionPhase::Refused,
            "The retention preview expired",
        );
    }
    let ids = preview
        .selection
        .candidates
        .iter()
        .map(|witness| witness.id.clone())
        .collect::<Vec<_>>();
    if tx.retention()?.retention_witnesses(&preview.window, &ids)? != preview.selection.candidates {
        return refused(
            tx,
            preview,
            RetentionPhase::Refused,
            "The selected records changed after preview",
        );
    }
    let outcome = tx
        .retention()?
        .apply_retention(&preview.window, &preview.selection.candidates)?;
    persist(tx, preview, RetentionPhase::Applied, outcome)
}

fn result(preview: &StoredRetention) -> EffectOutcome {
    match preview.phase {
        RetentionPhase::Applied => {
            let result = if preview.scheduled.is_some() {
                OperationResult::MaintenanceScheduledPrune {
                    value: inspection(preview),
                }
            } else {
                legacy_result(preview)
            };
            EffectOutcome::Completed(json!({"result":result}))
        }
        RetentionPhase::Cancelled | RetentionPhase::Refused => EffectOutcome::Failed(
            preview
                .outcome
                .as_ref()
                .and_then(|outcome| outcome.reason.as_ref())
                .cloned()
                .unwrap_or_else(|| "The retention operation did not run".into()),
        ),
        _ => EffectOutcome::Unknown("The retention outcome is unavailable".into()),
    }
}

impl JournaledEffect for MaintenanceService {
    fn resumes_after_observation(&self) -> bool {
        true
    }

    fn observes_delivery_cancellation(&self) -> bool {
        true
    }

    fn execute(&self, operation: &OperationRecord) -> EffectOutcome {
        self.execute_cancellable(operation, &Cancellation::default())
    }

    fn execute_cancellable(
        &self,
        operation: &OperationRecord,
        abandoned: &Cancellation,
    ) -> EffectOutcome {
        let Ok(job) = decode(operation) else {
            return EffectOutcome::Failed("Invalid operational retention effect".into());
        };
        let operation = operation.clone();
        let options = WriteOptions {
            cancellation: Some(abandoned.clone()),
            ..WriteOptions::host_state()
        };
        match run_write(&*self.store, options, move |tx| {
            execute_in(tx, &operation, &job)
        }) {
            Ok(saved) => result(&saved),
            Err(_) => EffectOutcome::Unknown("The retention outcome requires observation".into()),
        }
    }

    fn observe(&self, operation: &OperationRecord) -> EffectOutcome {
        let Ok(job) = decode(operation) else {
            return EffectOutcome::Failed("Invalid operational retention effect".into());
        };
        let observed = read_snapshot(&*self.store, &ReadOptions::default(), |read| {
            if read.stamp().store_id != job.store_id
                || read.stamp().restore_epoch != job.restore_epoch
                || read
                    .operations()
                    .operation(&operation.id)?
                    .is_none_or(|current| current.payload != operation.payload)
            {
                return Err(StorageError::ScopeDenied);
            }
            let preview = read
                .retention()?
                .retention_for_operation(&operation.id)?
                .filter(|preview| matches_preview(preview, operation, &job))
                .ok_or(StorageError::ScopeDenied)?;
            let entry = read
                .queue()
                .entry(&job.delivery)?
                .filter(|entry| delivery_is_bound(entry, &operation.id))
                .ok_or(StorageError::ScopeDenied)?;
            Ok((preview, entry.cancel_requested))
        });
        match observed {
            Ok((saved, _)) if saved.outcome.is_some() => result(&saved),
            Ok((saved, false)) if saved.phase == RetentionPhase::Queued => {
                EffectOutcome::NotApplied("The bound preview has no committed deletion".into())
            }
            Ok((saved, true)) if saved.phase == RetentionPhase::Queued => {
                let operation = operation.clone();
                match run_write(&*self.store, WriteOptions::settlement(), move |tx| {
                    let current = obtain(tx, &operation, &job)?;
                    if current.outcome.is_some() {
                        return Ok(current);
                    }
                    effect_job::attempt_holds(tx, &operation, EFFECT_CONSUMER)?;
                    if tx.queue().entry(&job.delivery)?.is_none_or(|entry| {
                        !delivery_is_bound(&entry, &operation.id) || !entry.cancel_requested
                    }) {
                        return Err(StorageError::ScopeDenied);
                    }
                    refused(
                        tx,
                        current,
                        RetentionPhase::Cancelled,
                        "Cancelled before removing records",
                    )
                }) {
                    Ok(saved) => result(&saved),
                    Err(_) => EffectOutcome::Unknown(
                        "The retention cancellation requires observation".into(),
                    ),
                }
            }
            _ => EffectOutcome::Unknown("The retention outcome is unavailable".into()),
        }
    }
}

pub(super) fn cancel(tx: &mut dyn WriteTransaction, id: &OperationId) -> Result<(), StorageError> {
    let operation = tx
        .operations()
        .operation(id)?
        .ok_or(StorageError::ScopeDenied)?;
    let job = decode(&operation)?;
    if obtain(tx, &operation, &job)?.outcome.is_some() {
        return Ok(());
    }
    let entry = tx
        .queue()
        .entry(&job.delivery)?
        .filter(|entry| delivery_is_bound(entry, id))
        .ok_or(StorageError::ScopeDenied)?;
    if !matches!(entry.state, QueueState::Completed | QueueState::Cancelled)
        && !entry.cancel_requested
    {
        let changed = tx.queue().cancel(&job.delivery)?;
        if changed.state == QueueState::Cancelled {
            cancelled(tx, &operation.id)?;
        }
    }
    Ok(())
}

fn cancelled(tx: &mut dyn WriteTransaction, id: &OperationId) -> Result<(), StorageError> {
    let operation = tx
        .operations()
        .operation(id)?
        .ok_or(StorageError::ScopeDenied)?;
    let job = decode(&operation)?;
    let preview = obtain(tx, &operation, &job)?;
    if operation.state != OperationState::CancelledBeforeDispatch {
        return Err(StorageError::ScopeDenied);
    }
    if preview.outcome.is_none() {
        refused(
            tx,
            preview,
            RetentionPhase::Cancelled,
            "Cancelled before removing records",
        )?;
    }
    Ok(())
}

pub(crate) struct QueueHook;

impl crate::services::queue::ConsumerHook for QueueHook {
    fn consumer(&self) -> &'static str {
        EFFECT_CONSUMER
    }

    fn cancelled(
        &self,
        tx: &mut dyn WriteTransaction,
        entry: &QueueEntry,
    ) -> Result<(), StorageError> {
        cancelled(
            tx,
            entry
                .operation_id
                .as_ref()
                .ok_or(StorageError::ScopeDenied)?,
        )
    }
}
