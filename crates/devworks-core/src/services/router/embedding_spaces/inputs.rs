use super::{
    EFFECT_CONSUMER, EmbeddingSpaceService, PROBE_KIND, PROBE_TEXT, PreparedEmbeddingProbe,
};
use crate::fingerprint::sha256_hex;
use crate::ports::storage::*;
use crate::protocol::OperationState;
use crate::protocol::provider::ArtifactRef;
use serde_json::{Value, json};

fn marker(p: &PreparedEmbeddingProbe) -> Value {
    json!({"kind":"EmbeddingProbeInput","candidateId":p.candidate_id(),"operationId":p.operation_id(),"requestId":p.request_id(),"admissionKey":p.admission_key()})
}
fn cached(
    operation: &OperationRecord,
    p: &PreparedEmbeddingProbe,
) -> Result<Option<Vec<ArtifactRef>>, StorageError> {
    if operation.id != *p.operation_id()
        || operation.capability != "router.configure"
        || operation.payload["kind"] != PROBE_KIND
    {
        return Err(StorageError::ScopeDenied);
    }
    let mut found: Option<Vec<ArtifactRef>> = None;
    for fact in operation
        .attempts
        .iter()
        .flat_map(|a| a.observations.iter())
        .filter_map(|o| o.get("embeddingProbeInputs"))
    {
        if fact["marker"] != marker(p) {
            continue;
        }
        let inputs: Vec<ArtifactRef> = serde_json::from_value(fact["inputs"].clone())
            .map_err(|_| StorageError::InvalidRecord("embedding probe input fact".into()))?;
        if found
            .as_ref()
            .is_some_and(|old| serde_json::to_value(old).ok() != serde_json::to_value(&inputs).ok())
        {
            return Err(StorageError::unavailable(
                Unavailable::Corrupt,
                "embedding probe input identity changed",
            ));
        }
        found = Some(inputs);
    }
    Ok(found)
}
fn validate_inputs(
    read: &dyn ArtifactRead,
    p: &PreparedEmbeddingProbe,
    inputs: &[ArtifactRef],
) -> Result<(), StorageError> {
    if inputs.len() != 1
        || inputs[0].sha256.0 != sha256_hex(PROBE_TEXT.as_bytes())
        || inputs[0].byte_length.get() != PROBE_TEXT.len() as i64
    {
        return Err(StorageError::InvalidRecord(
            "embedding probe must use its exact fixed input".into(),
        ));
    }
    let reference = &inputs[0];
    let artifact = read
        .artifact(&reference.artifact_id)?
        .ok_or(StorageError::ScopeDenied)?;
    let version = read
        .version(&reference.version_id)?
        .ok_or(StorageError::ScopeDenied)?;
    let scope = p
        .context()
        .project_id()
        .cloned()
        .map_or(ArtifactScope::Workspace, ArtifactScope::Project);
    if artifact.state != ArtifactState::Live
        || artifact.owner.as_ref() != Some(p.context().principal())
        || artifact.scope != scope
        || artifact.provenance["principalKind"] != "Host"
        || artifact.provenance["derived"] != marker(p)
        || !version.live()
        || version.artifact_id != reference.artifact_id
        || version.blob_digest != reference.sha256.0
        || version.byte_count != reference.byte_length.get()
        || version.media_type != reference.media_type
    {
        return Err(StorageError::ScopeDenied);
    }
    Ok(())
}
impl EmbeddingSpaceService {
    pub fn probe_input_marker(&self, p: &PreparedEmbeddingProbe) -> Value {
        marker(p)
    }
    pub fn probe_inputs(
        &self,
        p: &PreparedEmbeddingProbe,
    ) -> Result<Option<Vec<ArtifactRef>>, StorageError> {
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            let operation = read
                .operations()
                .operation(p.operation_id())?
                .ok_or(StorageError::ScopeDenied)?;
            let inputs = cached(&operation, p)?;
            if let Some(inputs) = &inputs {
                validate_inputs(read.artifacts(), p, inputs)?;
            }
            Ok(inputs)
        })
    }
    pub fn probe_inputs_in(
        &self,
        tx: &mut dyn WriteTransaction,
        p: &PreparedEmbeddingProbe,
    ) -> Result<Option<Vec<ArtifactRef>>, StorageError> {
        let operation = tx
            .operations()
            .operation(p.operation_id())?
            .ok_or(StorageError::ScopeDenied)?;
        let inputs = cached(&operation, p)?;
        if let Some(inputs) = &inputs {
            validate_inputs(&*tx.artifacts(), p, inputs)?;
        }
        Ok(inputs)
    }
    /// The native publisher calls this in the SAME writer unit as scoped Host publication. A
    /// competing winner must be read before publication; mismatched new refs roll this unit back.
    pub fn record_probe_inputs(
        &self,
        tx: &mut dyn WriteTransaction,
        p: &PreparedEmbeddingProbe,
        inputs: &[ArtifactRef],
    ) -> Result<(), StorageError> {
        let operation = tx
            .operations()
            .operation(p.operation_id())?
            .ok_or(StorageError::ScopeDenied)?;
        if let Some(old) = cached(&operation, p)? {
            validate_inputs(&*tx.artifacts(), p, &old)?;
            if serde_json::to_value(old).ok() != serde_json::to_value(inputs).ok() {
                return Err(StorageError::ScopeDenied);
            }
            return Ok(());
        }
        if operation.state != OperationState::Dispatching
            || operation
                .latest_attempt()
                .map(AttemptRecord::fence)
                .as_ref()
                != Some(p.operation_fence())
            || tx.store().store_id != *p.context().store_id()
            || tx.store().restore_epoch != p.context().restore_epoch().get()
            || !crate::authority::service::fences_hold(tx, &p.probe.authority_fences)?
        {
            return Err(StorageError::ScopeDenied);
        }
        let delivery = tx
            .queue()
            .entry(p.delivery_id())?
            .ok_or(StorageError::ScopeDenied)?;
        if delivery.consumer != EFFECT_CONSUMER
            || delivery.operation_id.as_ref() != Some(p.operation_id())
        {
            return Err(StorageError::ScopeDenied);
        }
        if delivery.cancel_requested {
            return Err(StorageError::Cancelled);
        }
        let current = tx.router().connection(&p.connection().id)?;
        if current.as_ref().map(|c| c.revision) != Some(p.connection().revision) {
            return Err(StorageError::ScopeDenied);
        }
        validate_inputs(&*tx.artifacts(), p, inputs)?;
        let reference = &inputs[0];
        crate::services::artifacts::reference_from_host_owner(
            tx,
            p.context(),
            reference,
            &SubjectKey {
                kind: "operation",
                id: p.operation_id().0.clone(),
            },
            None,
        )?;
        tx.operations().observe(
            p.operation_id(),
            p.operation_fence().attempt_no,
            json!({"embeddingProbeInputs":{"version":1,"marker":marker(p),"inputs":inputs}}),
        )?;
        Ok(())
    }
    pub fn probe_cancelled(&self, p: &PreparedEmbeddingProbe) -> Result<bool, StorageError> {
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            if read.stamp().store_id != *p.context().store_id()
                || read.stamp().restore_epoch != p.context().restore_epoch().get()
            {
                return Ok(true);
            }
            Ok(!read.queue().entry(p.delivery_id())?.is_some_and(|e| {
                e.consumer == EFFECT_CONSUMER
                    && e.operation_id.as_ref() == Some(p.operation_id())
                    && !e.cancel_requested
                    && e.state != QueueState::Cancelled
            }))
        })
    }
}
