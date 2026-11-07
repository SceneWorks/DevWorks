use super::{EmbeddingSpaceService, PROBE_TEXT, PreparedEmbeddingProbe};
use crate::fingerprint::sha256_hex;
use crate::ports::storage::*;
use crate::protocol::data::Metric;
use crate::protocol::provider::{ArtifactRef, EndpointProfile, NativeBinding};
use crate::protocol::{Id, OperationId};
use crate::services::router::accounting::AttemptSelection;
use serde_json::{Value, json};

/// Native host output, never a wire configuration argument. Byte buffers have already been
/// acquired through managed immutable-artifact reads outside the writer.
pub struct NativeProbeResult {
    pub request_id: Id,
    pub attempt_id: Id,
    pub result_artifact: ArtifactRef,
    pub vector_artifact: ArtifactRef,
    pub result_bytes: Vec<u8>,
    pub vector_bytes: Vec<u8>,
    pub reported_model: Option<String>,
}

/// Only raw immutable output verification can construct this fact. Its staging still requires
/// the actual completed native journal, local quiescence and original qualification fence.
pub struct VerifiedProbeEvidence {
    prepared: PreparedEmbeddingProbe,
    result: NativeProbeResult,
    selection: Value,
    binding: Value,
    registration_digest: String,
}
fn invalid(why: &str) -> StorageError {
    StorageError::InvalidRecord(why.into())
}
fn bytes_match(reference: &ArtifactRef, bytes: &[u8], media: &str) -> bool {
    reference.media_type == media
        && i64::try_from(bytes.len()).ok() == Some(reference.byte_length.get())
        && sha256_hex(bytes) == reference.sha256.0
}
impl EmbeddingSpaceService {
    pub fn verify_probe_result(
        &self,
        prepared: &PreparedEmbeddingProbe,
        result: NativeProbeResult,
    ) -> Result<VerifiedProbeEvidence, StorageError> {
        if result.request_id != *prepared.request_id()
            || result.result_bytes.len() > 1024 * 1024
            || !bytes_match(
                &result.result_artifact,
                &result.result_bytes,
                "application/vnd.devworks.embedding-result+json",
            )
            || !bytes_match(
                &result.vector_artifact,
                &result.vector_bytes,
                "application/vnd.devworks.float32-le",
            )
        {
            return Err(invalid("embedding probe immutable output mismatch"));
        }
        let manifest: Value = serde_json::from_slice(&result.result_bytes)
            .map_err(|_| invalid("embedding probe result manifest"))?;
        let binding: NativeBinding = serde_json::from_value(manifest["binding"].clone())
            .map_err(|_| invalid("embedding probe native binding"))?;
        let selection: AttemptSelection = serde_json::from_value(manifest["registration"].clone())
            .map_err(|_| invalid("embedding probe native selection"))?;
        let registration_digest = match &selection {
            AttemptSelection::Embedding {
                embedding_space_id,
                dimensions,
                upstream_model_id,
                connection_revision,
                registration_digest,
            } if embedding_space_id == &prepared.space().space_id
                && *dimensions == prepared.space().dimensions
                && upstream_model_id == prepared.upstream_model_id()
                && *connection_revision == prepared.connection().revision =>
            {
                registration_digest.clone()
            }
            _ => return Err(invalid("embedding probe selection mismatch")),
        };
        if binding.endpoint_profile != EndpointProfile::Embedding
            || binding.connection_id != prepared.connection().id
            || binding.account_revision.get() != prepared.connection().account_revision
            || binding.model_id
                != crate::services::router::connections::discovered_model_id(
                    "Embedding",
                    &prepared.connection().id,
                    prepared.upstream_model_id(),
                )
        {
            return Err(invalid("embedding probe account or model mismatch"));
        }
        if manifest["version"] != 1
            || manifest["kind"] != "EmbeddingResult"
            || manifest["requestId"] != json!(result.request_id)
            || manifest["attemptId"] != json!(result.attempt_id)
            || manifest["space"]
                != serde_json::to_value(prepared.space())
                    .map_err(|_| invalid("embedding space JSON"))?
            || manifest["dimensions"] != prepared.space().dimensions
        {
            return Err(invalid("embedding probe manifest identity mismatch"));
        }
        let inputs: Vec<ArtifactRef> = serde_json::from_value(manifest["inputs"].clone())
            .map_err(|_| invalid("embedding probe inputs"))?;
        if inputs.len() != 1
            || inputs[0].sha256.0 != sha256_hex(PROBE_TEXT.as_bytes())
            || inputs[0].byte_length.get() != PROBE_TEXT.len() as i64
        {
            return Err(invalid(
                "embedding qualification must use its exact host probe",
            ));
        }
        let vectors = manifest["vectors"]
            .as_array()
            .ok_or_else(|| invalid("embedding probe vector manifest"))?;
        if vectors.len() != 1
            || vectors[0]["index"] != 0
            || vectors[0]["sha256"] != result.vector_artifact.sha256.0
            || vectors[0]["byteCount"] != result.vector_bytes.len()
            || result.vector_bytes.len() != prepared.space().dimensions as usize * 4
        {
            return Err(invalid("embedding probe dimensions or population mismatch"));
        }
        let mut norm = 0.0_f64;
        for bytes in result.vector_bytes.as_chunks::<4>().0 {
            let value = f32::from_le_bytes(*bytes);
            if !value.is_finite() {
                return Err(invalid("embedding probe vector is not finite"));
            }
            norm += f64::from(value) * f64::from(value);
        }
        if matches!(prepared.space().metric, Metric::Cosine) && norm == 0.0 {
            return Err(invalid("zero-norm embedding cannot qualify a Cosine space"));
        }
        if result
            .reported_model
            .as_deref()
            .is_some_and(|m| m != prepared.upstream_model_id())
        {
            return Err(invalid("embedding provider reported a different model"));
        }
        Ok(VerifiedProbeEvidence {
            prepared: prepared.clone(),
            selection: manifest["registration"].clone(),
            binding: manifest["binding"].clone(),
            result,
            registration_digest,
        })
    }

    /// Stashes a verified fact before returning EffectResult::Completed. Dispatcher transaction B
    /// can then atomically finish qualification from the retained fact, without reading files or
    /// trusting a caller-authored reconciliation payload.
    pub fn persist_verified_probe_evidence(
        &self,
        verified: VerifiedProbeEvidence,
    ) -> Result<(), StorageError> {
        run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
            persist_verified_in(tx, verified)
        })
    }
    /// The native owner checks its atomic close fence inside this writer unit, before calling.
    pub fn persist_verified_probe_evidence_in(
        &self,
        tx: &mut dyn WriteTransaction,
        verified: VerifiedProbeEvidence,
    ) -> Result<(), StorageError> {
        persist_verified_in(tx, verified)
    }

    /// Runs inside dispatcher transaction B after the actual operation transitioned complete.
    pub fn complete_probe(
        &self,
        tx: &mut dyn WriteTransaction,
        operation_id: &OperationId,
    ) -> Result<(), StorageError> {
        let operation = tx
            .operations()
            .operation(operation_id)?
            .ok_or(StorageError::ScopeDenied)?;
        if operation.capability != "router.configure"
            || operation.payload["kind"] != super::PROBE_KIND
        {
            return Err(StorageError::ScopeDenied);
        }
        let id: Id = serde_json::from_value(operation.payload["candidateId"].clone())
            .map_err(|_| invalid("embedding candidate identity"))?;
        let q = tx
            .router()
            .embedding_qualification(&id)?
            .ok_or(StorageError::ScopeDenied)?;
        if matches!(
            q.state,
            EmbeddingQualificationState::Qualified | EmbeddingQualificationState::Cancelled
        ) && operation.state == crate::protocol::OperationState::ObservedComplete
            && q.probe
                .as_ref()
                .is_some_and(|p| p.operation_id == operation.id)
        {
            return Ok(());
        }
        let fence = operation
            .latest_attempt()
            .ok_or(StorageError::ScopeDenied)?
            .fence();
        let delivery_id = q
            .probe
            .as_ref()
            .ok_or(StorageError::ScopeDenied)?
            .delivery_id
            .clone();
        let cancelled = tx
            .queue()
            .entry(&delivery_id)?
            .is_some_and(|e| e.cancel_requested || e.state == QueueState::Cancelled);
        tx.router().finish_embedding_qualification(
            &id,
            q.revision,
            &operation.id,
            Some(&fence),
            if cancelled {
                EmbeddingQualificationState::Cancelled
            } else {
                EmbeddingQualificationState::Qualified
            },
            cancelled.then_some("The qualification was cancelled before publication."),
        )?;
        Ok(())
    }
}

fn persist_verified_in(
    tx: &mut dyn WriteTransaction,
    verified: VerifiedProbeEvidence,
) -> Result<(), StorageError> {
    let p = &verified.prepared;
    let q = tx
        .router()
        .embedding_qualification(p.candidate_id())?
        .ok_or(StorageError::ScopeDenied)?;
    let attempt = tx
        .usage()
        .inference_attempt(&verified.result.attempt_id)?
        .ok_or(StorageError::ScopeDenied)?;
    let snapshot = crate::services::usage::accounting::snapshot(&attempt)?;
    if serde_json::to_value(&snapshot.selection).map_err(|_| invalid("native selection JSON"))?
        != verified.selection
        || snapshot.admission_key != *p.admission_key()
    {
        return Err(StorageError::ScopeDenied);
    }
    for reference in [
        &verified.result.result_artifact,
        &verified.result.vector_artifact,
    ] {
        let artifact = tx
            .artifacts()
            .artifact(&reference.artifact_id)?
            .ok_or(StorageError::ScopeDenied)?;
        let derived = &artifact.provenance["derived"];
        if artifact.provenance["principalKind"] != "Host"
            || derived["requestId"] != json!(verified.result.request_id)
            || derived["attemptId"] != json!(verified.result.attempt_id)
            || derived["binding"] != verified.binding
        {
            return Err(StorageError::ScopeDenied);
        }
    }
    let evidence = StoredEmbeddingEvidence {
        request_id: verified.result.request_id,
        native_operation_id: attempt.operation_id.clone(),
        attempt_id: verified.result.attempt_id,
        result_artifact: verified.result.result_artifact,
        vector_artifact: verified.result.vector_artifact,
        reported_model: verified.result.reported_model,
        registration_digest: verified.registration_digest,
        connection_revision: p.connection().revision,
    };
    tx.router().stage_embedding_probe_evidence(
        p.candidate_id(),
        q.revision,
        p.operation_id(),
        p.operation_fence(),
        &evidence,
    )?;
    for reference in [&evidence.result_artifact, &evidence.vector_artifact] {
        for operation in [p.operation_id(), &evidence.native_operation_id] {
            crate::services::artifacts::reference_from_host_owner(
                tx,
                p.context(),
                reference,
                &SubjectKey {
                    kind: "operation",
                    id: operation.0.clone(),
                },
                None,
            )?;
        }
    }
    Ok(())
}
