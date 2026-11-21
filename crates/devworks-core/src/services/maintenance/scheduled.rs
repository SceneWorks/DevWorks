//! A native Jobs firing can capture exactly one preview and one child operation.
use super::{DAY_MS, SCHEDULED_OPERATION, human, record_preview, worker};
use crate::context::{InvocationContext, PrincipalKind};
use crate::fingerprint::sha256_hex;
use crate::gateway::AdmittedCommand;
use crate::operations::maintenance::ScheduledPruneInput;
use crate::ports::storage::{
    RetentionKind, ScheduledRetention, StorageError, StoredJobRun, WriteTransaction,
};
use crate::protocol::records::{JobDefinition, PruneKind};
use crate::protocol::results::DomainDecision;
use crate::protocol::{ActorId, OperationId, ProjectId};
use crate::services::{diagnostics, jobs, kernel::Failure};
use serde_json::Value;

fn target(kind: &PruneKind) -> RetentionKind {
    match kind {
        PruneKind::Diagnostics => RetentionKind::Diagnostics,
        PruneKind::IndexRuns => RetentionKind::IndexRuns,
        PruneKind::Messages => RetentionKind::Messages,
        PruneKind::Sessions => RetentionKind::Sessions,
    }
}

fn parse(args: Value) -> Result<ScheduledPruneInput, StorageError> {
    let input: ScheduledPruneInput = serde_json::from_value(args)
        .map_err(|_| StorageError::InvalidRecord("invalid retention job arguments".into()))?;
    if matches!(input.target, PruneKind::Diagnostics) && input.retention_days.is_some() {
        return Err(StorageError::InvalidRecord(
            "diagnostics jobs use the current workspace retention age".into(),
        ));
    }
    Ok(input)
}

fn arguments(definition: &str) -> Result<Option<ScheduledPruneInput>, StorageError> {
    let definition: JobDefinition = serde_json::from_str(definition)
        .map_err(|_| StorageError::InvalidRecord("invalid native job definition".into()))?;
    match definition {
        JobDefinition::Native {
            capability_id,
            arguments,
            ..
        } if capability_id == jobs::capability_id(SCHEDULED_OPERATION) => {
            parse(arguments.value.0).map(Some)
        }
        _ => Ok(None),
    }
}

pub(crate) fn is_retention_job(definition: &str) -> Result<bool, StorageError> {
    Ok(arguments(definition)?.is_some())
}

pub(crate) fn validate_schedule(
    ctx: &InvocationContext,
    project: Option<&ProjectId>,
    args: &Value,
) -> Result<(), StorageError> {
    human(ctx).map_err(Failure::into_storage)?;
    if project.is_some() {
        return Err(StorageError::ScopeDenied);
    }
    parse(args.clone())?;
    Ok(())
}

pub(super) fn authorization(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    operation: Option<&OperationId>,
) -> Result<(ActorId, ScheduledRetention, ScheduledPruneInput), StorageError> {
    let denied = StorageError::ScopeDenied;
    if ctx.principal_kind() != PrincipalKind::Host
        || ctx.principal() != &tx.store().host_actor
        || ctx.project_id().is_some()
        || ctx.root_id().is_some()
        || ctx.assignment_id().is_some()
    {
        return Err(denied);
    }
    let run_id = ctx.job_run_id().ok_or_else(|| denied.clone())?;
    let run = tx.jobs().job_run(run_id)?.ok_or_else(|| denied.clone())?;
    let job = tx.jobs().job(&run.job_id)?.ok_or_else(|| denied.clone())?;
    let epoch = tx.store().restore_epoch;
    if run.project_id.is_some()
        || run.restore_epoch != epoch
        || run.boot_id.as_ref() != tx.store().boot_id.as_ref()
        || run.cancel_requested
        || !matches!(run.status.as_str(), "Running" | "Holding")
        || operation.is_some_and(|expected| {
            run.inner_operation_id
                .as_ref()
                .is_some_and(|id| id != expected)
        })
        || job.project_id.is_some()
        || job.activated_epoch != epoch
        || !job.enabled
        || !job.authorized
        || job.hold.is_some()
        || job.deleted_at_ms.is_some()
        || job.revision != run.job_revision
        || job.definition != run.definition
    {
        return Err(denied);
    }
    let owner = job.authorized_by.ok_or_else(|| denied.clone())?;
    if tx.actors().actor_kind(&owner)? != Some(PrincipalKind::Human) {
        return Err(denied);
    }
    let input = arguments(&run.definition)?.ok_or(denied)?;
    let binding = ScheduledRetention {
        job_id: job.id,
        run_id: run.id,
        job_revision: run.job_revision,
        definition_digest: sha256_hex(run.definition.as_bytes()),
    };
    Ok((owner, binding, input))
}

pub(super) fn diagnostics_cutoff(
    tx: &mut dyn WriteTransaction,
    at_ms: i64,
) -> Result<i64, StorageError> {
    let days = diagnostics::retention(tx.configuration())?.days;
    Ok(at_ms.saturating_sub(days * DAY_MS).max(0))
}

pub(super) fn fire(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    command: &AdmittedCommand,
    input: &ScheduledPruneInput,
) -> Result<DomainDecision, Failure> {
    let (owner, binding, approved) = authorization(tx, ctx, None)?;
    let kind = target(&input.target);
    if kind != target(&approved.target)
        || input.max_items.get() != approved.max_items.get()
        || input.retention_days.as_ref().map(|days| days.get())
            != approved.retention_days.as_ref().map(|days| days.get())
    {
        return Err(StorageError::ScopeDenied.into());
    }
    if let Some(saved) = tx.retention()?.retention_for_job_run(&binding.run_id)? {
        if saved.owner != owner
            || saved.scheduled.as_ref() != Some(&binding)
            || saved.store != tx.store().store_id
            || saved.restore_epoch != tx.store().restore_epoch
        {
            return Err(StorageError::ScopeDenied.into());
        }
        return Ok(DomainDecision::Accepted {
            operation_id: saved.operation.ok_or(StorageError::ScopeDenied)?,
        });
    }
    let now = tx.now_ms();
    let cutoff = if kind == RetentionKind::Diagnostics {
        diagnostics_cutoff(tx, now)?
    } else {
        let days = approved
            .retention_days
            .as_ref()
            .map_or(30, |days| i64::from(days.get()));
        now.saturating_sub(days * DAY_MS).max(0)
    };
    let preview = record_preview(
        tx,
        owner,
        kind,
        cutoff,
        approved.max_items.get() as u16,
        Some(binding),
    )?;
    worker::enqueue(tx, ctx, command, preview)
}

pub(crate) fn cancel_scheduled(
    tx: &mut dyn WriteTransaction,
    run: &StoredJobRun,
) -> Result<(), StorageError> {
    if arguments(&run.definition)?.is_none() {
        return Ok(());
    }
    if let Some(saved) = tx.retention()?.retention_for_job_run(&run.id)? {
        if saved.scheduled.as_ref().is_none_or(|binding| {
            binding.job_id != run.job_id
                || binding.job_revision != run.job_revision
                || binding.definition_digest != sha256_hex(run.definition.as_bytes())
        }) || run
            .inner_operation_id
            .as_ref()
            .is_some_and(|id| saved.operation.as_ref() != Some(id))
        {
            return Err(StorageError::ScopeDenied);
        }
        let operation = saved.operation.as_ref().ok_or(StorageError::ScopeDenied)?;
        worker::cancel(tx, operation)?;
    }
    Ok(())
}
