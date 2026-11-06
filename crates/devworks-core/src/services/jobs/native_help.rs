//! Retained empty maintenance-job arguments capture current owner metadata once per actual run.
//! Manual commands retain their own explicit argument contracts.
use super::*;
use crate::ports::storage::{KnowledgeScope, WriteTransaction};
use crate::protocol::ActorId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(super) const OPERATION: &str = "help.generate_capability_docs";
pub(super) const CONVENTION_INDEX: &str = "conventions.index";
pub(super) fn supports(operation: &str, revision: SchemaRevision) -> bool {
    operation == OPERATION || operation == CONVENTION_INDEX && revision.0 >= 3
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyJobRequest {}

/// Additive schema identity: existing explicit command-argument references remain immutable.
pub(super) fn argument_schema(operation: &str, revision: SchemaRevision) -> Option<SchemaRef> {
    if !supports(operation, revision) {
        return None;
    }
    let descriptor = registry::find(operation)?;
    let mut explicit =
        serde_json::to_value(descriptor.binding(Mode::Command)?.input_schema(revision)).ok()?;
    // References in the selected input schema remain rooted at the combined document.
    let definitions = explicit.as_object_mut()?.remove("$defs");
    let mut schema = json!({"anyOf":[schemars::schema_for!(EmptyJobRequest),explicit]});
    if let Some(definitions) = definitions {
        schema["$defs"] = definitions;
    }
    let canonical = crate::jcs::canonicalize(&schema).ok()?;
    Some(SchemaRef {
        schema_id: QualifiedId::try_from(format!(
            "{ARGUMENT_SCHEMA_NAMESPACE}/{operation}-current"
        ))
        .ok()?,
        revision: Revision::new(i64::from(revision.0)).ok()?,
        sha256: Digest(sha256_hex(canonical.as_bytes())),
    })
}

pub(super) fn is_empty(value: &Value) -> bool {
    value.as_object().is_some_and(|m| m.is_empty())
}

/// Cancellation follows the actual Accepted child operation, never a reconstructed request
/// key. The Help owner validates its own immutable delivery binding before touching the queue.
pub(super) fn cancel_accepted(
    tx: &mut dyn WriteTransaction,
    run: &StoredJobRun,
) -> Result<(), StorageError> {
    let definition: JobDefinition = super::parse(&run.definition)?;
    if matches!(definition,JobDefinition::Native{capability_id:ref id,..} if id==&super::capability_id(OPERATION))
        && let Some(operation) = &run.inner_operation_id
    {
        crate::services::help::request_generation_cancel(tx, operation)?;
    }
    if matches!(definition,JobDefinition::Native{capability_id:ref id,..} if id==&super::capability_id(CONVENTION_INDEX))
        && let Some(operation) = &run.inner_operation_id
    {
        crate::services::conventions::indexing::cancellation::request_cancel(tx, operation)?;
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CapturedInvocation {
    version: u8,
    store_id: crate::protocol::StoreId,
    host_actor: ActorId,
    job_id: Id,
    run_id: Id,
    project_id: Option<ProjectId>,
    restore_epoch: i64,
    invocation_id: Id,
    frame: String,
}

/// Capture once in begin's writer UoW. A resumed run always uses its exact prior frame and
/// invocation identity, even after Help/registry changes or an ambiguous acknowledgment.
pub(super) fn invocation(
    tx: &mut dyn WriteTransaction,
    run: &mut StoredJobRun,
    operation: &str,
    revision: i64,
) -> Result<(Id, Vec<u8>), StorageError> {
    let invalid = || StorageError::InvalidRecord("invalid captured Help job invocation".into());
    if let Some(saved) = &run.checkpoint {
        let saved: CapturedInvocation = serde_json::from_str(saved).map_err(|_| invalid())?;
        if saved.version != 1
            || saved.store_id != tx.store().store_id
            || saved.host_actor != tx.store().host_actor
            || saved.job_id != run.job_id
            || saved.run_id != run.id
            || saved.project_id != run.project_id
            || saved.restore_epoch != run.restore_epoch
        {
            return Err(invalid());
        }
        let frame: Value = serde_json::from_str(&saved.frame).map_err(|_| invalid())?;
        if frame["protocol"] != crate::protocol::CONTRACT
            || frame["mode"] != "Command"
            || frame["requestKey"] != json!(run.id)
            || frame["schemaRevision"] != revision
            || frame["body"]["kind"] != operation
        {
            return Err(invalid());
        }
        return Ok((saved.invocation_id, saved.frame.into_bytes()));
    }
    let mut body = if operation == OPERATION {
        serde_json::to_value(crate::services::help::current_generation_args_in(tx)?)
            .map_err(|_| invalid())?
    } else if operation == CONVENTION_INDEX && revision >= 3 {
        let scope = run
            .project_id
            .clone()
            .map_or(crate::protocol::data::DataScope::Workspace, |project_id| {
                crate::protocol::data::DataScope::Project { project_id }
            });
        let stored = run
            .project_id
            .clone()
            .map_or(KnowledgeScope::Workspace, KnowledgeScope::Project);
        let current = tx.conventions().convention_scope_revision(&stored)?;
        json!({"allScope":scope,"expectedRevision":current})
    } else {
        return Err(invalid());
    };
    body.as_object_mut()
        .ok_or_else(invalid)?
        .insert("kind".into(), json!(operation));
    let frame = json!({"protocol":crate::protocol::CONTRACT,"mode":"Command",
        "requestKey":run.id,"schemaRevision":revision,"body":body})
    .to_string();
    let invocation_id = tx.new_id(IdKind::Correlation);
    let saved = CapturedInvocation {
        version: 1,
        store_id: tx.store().store_id.clone(),
        host_actor: tx.store().host_actor.clone(),
        job_id: run.job_id.clone(),
        run_id: run.id.clone(),
        project_id: run.project_id.clone(),
        restore_epoch: run.restore_epoch,
        invocation_id: invocation_id.clone(),
        frame: frame.clone(),
    };
    let saved = serde_json::to_string(&saved).map_err(|_| invalid())?;
    tx.jobs().set_run_checkpoint(&run.id, &saved)?;
    run.checkpoint = Some(saved);
    Ok((invocation_id, frame.into_bytes()))
}
