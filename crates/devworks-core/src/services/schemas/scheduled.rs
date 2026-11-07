//! Human-authorized metadata-only native schedules. Provenance alone grants nothing.
use crate::{
    context::{InvocationContext, PrincipalKind},
    ports::{IdKind, storage::*},
    protocol::{ActorId, Id, ProjectId, StoreId, records::JobDefinition},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const OPERATION: &str = "schemas.index";
#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CurrentSource {
    pub connection_id: Id,
}
pub(crate) fn current(value: &Value) -> Option<CurrentSource> {
    serde_json::from_value(value.clone()).ok()
}
fn invalid() -> StorageError {
    StorageError::InvalidRecord("invalid schema schedule binding".into())
}
fn target(definition: &str) -> Result<Option<CurrentSource>, StorageError> {
    let definition: JobDefinition = serde_json::from_str(definition).map_err(|_| invalid())?;
    Ok(match definition {
        JobDefinition::Native {
            capability_id,
            arguments,
        } if capability_id == crate::services::jobs::capability_id(OPERATION)
            && arguments.schema.revision.get() >= 5 =>
        {
            current(&arguments.value.0)
        }
        _ => None,
    })
}
fn allowed(source: &StoredSchemaSource, by: &ActorId, project: Option<&ProjectId>) -> bool {
    !source.deleted
        && source.connection.config.as_ref().is_some_and(|c| {
            c.enabled != Some(false)
                && match project {
                    None => source.owner_id == *by || c.workspace_exposed == Some(true),
                    Some(project) => c.project_access.as_ref().is_some_and(|a| {
                        a.0.iter()
                            .any(|a| a.project_id == project.0 && a.metadata_enabled)
                    }),
                }
        })
}
pub(crate) fn validate_definition(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    project: Option<&ProjectId>,
    source: &CurrentSource,
) -> Result<(), StorageError> {
    let by = ctx
        .reviewed_operation()
        .map_or_else(|| ctx.principal(), |r| r.approver());
    let source = tx
        .schemas()
        .schema_source(&source.connection_id)?
        .ok_or(StorageError::ScopeDenied)?;
    if !allowed(&source, by, project) {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Captured {
    version: u8,
    store_id: StoreId,
    restore_epoch: i64,
    host_actor: ActorId,
    authorizer: ActorId,
    job_id: Id,
    run_id: Id,
    project_id: Option<ProjectId>,
    connection_id: Id,
    definition_digest: String,
    invocation_id: Option<Id>,
    frame: Option<String>,
}
pub(crate) fn queue_checkpoint(
    tx: &mut dyn WriteTransaction,
    job: &StoredJob,
    run: &StoredJobRun,
    by_person: bool,
    actor: &ActorId,
) -> Result<Option<String>, StorageError> {
    let Some(target) = target(&run.definition)? else {
        return Ok(None);
    };
    let by = if by_person {
        Some(actor)
    } else {
        job.authorized_by.as_ref()
    }
    .ok_or(StorageError::ScopeDenied)?;
    if tx.actors().actor_kind(by)? != Some(PrincipalKind::Human) {
        return Err(StorageError::ScopeDenied);
    }
    serde_json::to_string(&Captured {
        version: 1,
        store_id: tx.store().store_id.clone(),
        restore_epoch: run.restore_epoch,
        host_actor: tx.store().host_actor.clone(),
        authorizer: by.clone(),
        job_id: run.job_id.clone(),
        run_id: run.id.clone(),
        project_id: run.project_id.clone(),
        connection_id: target.connection_id,
        definition_digest: crate::fingerprint::sha256_hex(run.definition.as_bytes()),
        invocation_id: None,
        frame: None,
    })
    .map(Some)
    .map_err(|_| invalid())
}
fn captured(tx: &mut dyn WriteTransaction, run: &StoredJobRun) -> Result<Captured, StorageError> {
    let c: Captured = serde_json::from_str(run.checkpoint.as_deref().ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
    let target = target(&run.definition)?.ok_or_else(invalid)?;
    if c.version != 1
        || c.store_id != tx.store().store_id
        || c.restore_epoch != tx.store().restore_epoch
        || c.restore_epoch != run.restore_epoch
        || c.host_actor != tx.store().host_actor
        || c.job_id != run.job_id
        || c.run_id != run.id
        || c.project_id != run.project_id
        || c.connection_id != target.connection_id
        || c.definition_digest != crate::fingerprint::sha256_hex(run.definition.as_bytes())
        || tx.actors().actor_kind(&c.authorizer)? != Some(PrincipalKind::Human)
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(c)
}
pub(crate) fn invocation(
    tx: &mut dyn WriteTransaction,
    run: &mut StoredJobRun,
    revision: i64,
) -> Result<(Id, Vec<u8>), StorageError> {
    let mut c = captured(tx, run)?;
    if let (Some(invocation), Some(frame)) = (&c.invocation_id, &c.frame) {
        let value: Value = serde_json::from_str(frame).map_err(|_| invalid())?;
        if value["requestKey"] != json!(run.id)
            || value["schemaRevision"] != revision
            || value["protocol"] != crate::protocol::CONTRACT
            || value["mode"] != "Command"
            || value["body"]["kind"] != OPERATION
            || value["body"]["connectionId"] != json!(c.connection_id)
        {
            return Err(invalid());
        }
        return Ok((invocation.clone(), frame.as_bytes().to_vec()));
    }
    if c.invocation_id.is_some() || c.frame.is_some() {
        return Err(invalid());
    }
    let source = tx
        .schemas()
        .schema_source(&c.connection_id)?
        .ok_or(StorageError::ScopeDenied)?;
    if !allowed(&source, &c.authorizer, c.project_id.as_ref()) {
        return Err(StorageError::ScopeDenied);
    }
    let scope = c
        .project_id
        .clone()
        .map_or(crate::protocol::data::DataScope::Workspace, |project_id| {
            crate::protocol::data::DataScope::Project { project_id }
        });
    let frame = json!({"protocol":crate::protocol::CONTRACT,"mode":"Command","requestKey":run.id,
        "schemaRevision":revision,"body":{"kind":OPERATION,"connectionId":c.connection_id,
        "expectedRevision":source.connection.revision,"scope":scope}})
    .to_string();
    let invocation = tx.new_id(IdKind::Correlation);
    c.invocation_id = Some(invocation.clone());
    c.frame = Some(frame.clone());
    let saved = serde_json::to_string(&c).map_err(|_| invalid())?;
    tx.jobs().set_run_checkpoint(&run.id, &saved)?;
    run.checkpoint = Some(saved);
    Ok((invocation, frame.into_bytes()))
}
/// Supplement only metadata indexing authority, never source discovery/configuration or row access.
pub(super) fn permits(
    tx: &mut dyn WriteTransaction,
    ctx: &InvocationContext,
    source: &StoredSchemaSource,
    scope: &crate::protocol::data::DataScope,
) -> Result<bool, StorageError> {
    if ctx.principal_kind() != PrincipalKind::Host || ctx.principal() != &tx.store().host_actor {
        return Ok(false);
    }
    let Some(run_id) = ctx.job_run_id() else {
        return Ok(false);
    };
    let Some(run) = tx.jobs().job_run(run_id)? else {
        return Ok(false);
    };
    if target(&run.definition)?.is_none() {
        return Ok(false);
    }
    let c = captured(tx, &run)?;
    let expected_scope = c
        .project_id
        .clone()
        .map_or(crate::protocol::data::DataScope::Workspace, |project_id| {
            crate::protocol::data::DataScope::Project { project_id }
        });
    Ok(run.status == "Running"
        && !run.cancel_requested
        && c.connection_id == source.connection.id
        && c.invocation_id.as_ref() == Some(ctx.invocation_id())
        && c.frame.is_some()
        && serde_json::to_value(scope).ok() == serde_json::to_value(expected_scope).ok()
        && ctx.project_id() == c.project_id.as_ref()
        && allowed(source, &c.authorizer, c.project_id.as_ref()))
}
pub(crate) fn cancel_accepted(
    tx: &mut dyn WriteTransaction,
    run: &StoredJobRun,
) -> Result<(), StorageError> {
    let Some(inner) = &run.inner_operation_id else {
        return Ok(());
    };
    let Some(operation) = tx.operations().operation(inner)? else {
        return Err(invalid());
    };
    let job: super::indexing::SchemaJob =
        serde_json::from_value(operation.payload.clone()).map_err(|_| invalid())?;
    if operation.capability != OPERATION
        || job.kind != super::indexing::JOB_KIND
        || job.source_context["invocationContext"]["jobRun"] != json!(run.id)
        || job.actor != tx.store().host_actor
        || job.store_id != tx.store().store_id
        || job.restore_epoch != run.restore_epoch
    {
        return Err(StorageError::ScopeDenied);
    }
    let entry = tx.queue().entry(&job.delivery_id)?.ok_or_else(invalid)?;
    if entry.consumer != super::indexing::EFFECT_CONSUMER
        || entry.operation_id.as_ref() != Some(inner)
    {
        return Err(StorageError::ScopeDenied);
    }
    if !entry.cancel_requested {
        tx.queue().cancel(&job.delivery_id)?;
    }
    super::indexing::settle_cancelled(tx, inner)?;
    Ok(())
}
