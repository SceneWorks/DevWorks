//! Full private review snapshots with immutable, human-readable evidence. Expansion checks
//! current artifact metadata and retained bytes; it performs no filesystem I/O in a writer.
use super::{OwnerPreview, owner_preview::artifact_binding};
use crate::ports::storage::{ArtifactRead, ConfigScope, StorageError};
use crate::protocol::provider::ArtifactRef;
use crate::protocol::{DomainErrorCode, Id};
use crate::services::sessions::{Failure, Refusal};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewDocument {
    scope_key: String,
    target_id: Id,
    target_revision: i64,
    summary: String,
    reviewed: Value,
    binding: Value,
}
/// The complete document that the existing evidence publisher stores before admission.
pub(crate) fn document(preview: &OwnerPreview) -> Value {
    serde_json::to_value(ReviewDocument {
        scope_key: preview.scope.key(),
        target_id: preview.target_id.clone(),
        target_revision: preview.target_revision,
        summary: preview.summary.clone(),
        reviewed: preview.reviewed.clone(),
        binding: preview.binding.clone(),
    })
    .expect("owner review document")
}
/// Host metadata can contain exact integers outside JCS's interoperable number range. As with
/// the private scope digest, preserve those exact JSON integers instead of rounding them.
pub(crate) fn bytes(document: &Value) -> Vec<u8> {
    crate::jcs::canonicalize(document)
        .unwrap_or_else(|_| document.to_string())
        .into_bytes()
}
pub(crate) fn needed(preview: &OwnerPreview) -> bool {
    serde_json::from_value::<crate::protocol::support::DynamicJson>(preview.reviewed.clone())
        .is_err()
}

fn changed() -> Failure {
    Refusal::new(DomainErrorCode::SourceChanged, "review_evidence", None)
        .retry(crate::protocol::RetryAdvice::RefreshAndNewRequest)
        .into()
}
/// The private document stays in the authoritative proposal; the bounded public projection
/// points to its complete immutable evidence. No caller-owned JSON is interpreted as a marker.
pub(crate) fn seal(
    read: &dyn ArtifactRead,
    preview: &OwnerPreview,
    reference: &ArtifactRef,
) -> Result<OwnerPreview, Failure> {
    let document = document(preview);
    let encoded = bytes(&document);
    if reference.media_type != "application/json"
        || reference.byte_length.get() != encoded.len() as i64
        || reference.sha256.0 != crate::fingerprint::sha256_hex(&encoded)
    {
        return Err(changed());
    }
    let witness = artifact_binding(read, reference)?;
    Ok(OwnerPreview {
        scope: preview.scope.clone(),
        target_id: preview.target_id.clone(),
        target_revision: preview.target_revision,
        summary: preview.summary.clone(),
        reviewed: json!({"artifact":reference,"summary":preview.summary,"reviewDigest":reference.sha256}),
        binding: json!({"reviewEnvelope":{"version":1,"artifact":witness,"document":document}}),
    })
}
/// Restore the exact historical owner inputs after validating their immutable artifact and
/// identity. The private full document makes restart/rejection safe without a transient cache.
pub(crate) fn expand(
    read: &dyn ArtifactRead,
    preview: &OwnerPreview,
) -> Result<OwnerPreview, Failure> {
    let Some(envelope) = preview.binding.get("reviewEnvelope") else {
        return Ok(preview.clone());
    };
    if envelope["version"].as_u64() != Some(1) {
        return Err(changed());
    }
    let reference: ArtifactRef =
        serde_json::from_value(envelope["artifact"]["reference"].clone()).map_err(|_| changed())?;
    if artifact_binding(read, &reference)? != envelope["artifact"] {
        return Err(changed());
    }
    let document = envelope["document"].clone();
    let encoded = bytes(&document);
    if reference.media_type != "application/json"
        || reference.byte_length.get() != encoded.len() as i64
        || reference.sha256.0 != crate::fingerprint::sha256_hex(&encoded)
    {
        return Err(changed());
    }
    let raw: ReviewDocument = serde_json::from_value(document)
        .map_err(|e| StorageError::InvalidRecord(format!("owner review document: {e}")))?;
    let scope = ConfigScope::from_key(&raw.scope_key).ok_or_else(changed)?;
    if scope != preview.scope
        || raw.target_id != preview.target_id
        || raw.target_revision != preview.target_revision
        || raw.summary != preview.summary
        || raw.target_revision < 1
    {
        return Err(changed());
    }
    Ok(OwnerPreview {
        scope,
        target_id: raw.target_id,
        target_revision: raw.target_revision,
        summary: raw.summary,
        reviewed: raw.reviewed,
        binding: raw.binding,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn evidence_is_needed_for_shape_limits_even_when_contents_are_small() {
        let mut value = json!("leaf");
        for _ in 0..64 {
            value = json!([value]);
        }
        let preview = OwnerPreview {
            scope: ConfigScope::Workspace,
            target_id: Id::new("job").unwrap(),
            target_revision: 1,
            summary: "Review job".into(),
            reviewed: json!({"arguments":value}),
            binding: Value::Null,
        };
        assert!(serde_json::to_vec(&preview.reviewed).unwrap().len() < 512);
        assert!(needed(&preview));
    }
    #[test]
    fn full_review_document_keeps_large_historical_contents_and_exact_host_numbers() {
        let preview = OwnerPreview {
            scope: ConfigScope::Workspace,
            target_id: Id::new("job").unwrap(),
            target_revision: 1,
            summary: "Review job".into(),
            reviewed: json!({"arguments":"x".repeat(524_289)}),
            binding: json!({"fileId":u64::MAX}),
        };
        assert!(needed(&preview));
        let value = document(&preview);
        let raw: ReviewDocument = serde_json::from_slice(&bytes(&value)).unwrap();
        assert_eq!(raw.reviewed, preview.reviewed);
        assert_eq!(raw.binding, preview.binding);
        assert_eq!(
            super::super::model::hash(&value),
            crate::fingerprint::sha256_hex(&bytes(&value))
        );
    }
}
