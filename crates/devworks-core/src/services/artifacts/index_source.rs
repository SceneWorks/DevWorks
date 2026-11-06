//! Private captured index bytes. These are not public artifacts or artifact versions.
use super::{ArtifactService, StagedFile, put_staged_blob};
use crate::fingerprint::sha256_hex;
use crate::ports::storage::{
    ReadSnapshot, StorageError, StoredGraphSourceCapture, WriteTransaction,
};
use crate::protocol::Id;
use serde_json::json;
use std::io::Read;

impl ArtifactService {
    /// The index publisher calls this in the same transaction as `put_unit`. Its staging
    /// operation ID is already unique and becomes the private capture identity.
    pub(crate) fn commit_index_source(
        &self,
        tx: &mut dyn WriteTransaction,
        staged: &StagedFile,
        repository: &Id,
        unit_key: &str,
        source_digest: &str,
    ) -> Result<StoredGraphSourceCapture, StorageError> {
        let generation = put_staged_blob(tx, &*self.files, staged)?;
        let capture = StoredGraphSourceCapture {
            id: staged.operation_id.0.clone(),
            repository_id: repository.clone(),
            unit_key: unit_key.to_owned(),
            source_digest: source_digest.to_owned(),
            blob_digest: staged.digest.clone(),
            blob_generation: generation,
            byte_count: staged.byte_count,
            created_at_ms: tx.now_ms(),
        };
        tx.graph().insert_graph_source_capture(&capture)?;
        super::complete_operation(
            tx,
            &staged.operation_id,
            json!({"indexSourceCaptureId": capture.id}),
        )?;
        Ok(capture)
    }

    /// Opens only the private capture already resolved and authorized by the graph service.
    /// The caller must revalidate its exact view/unit binding when publishing the response.
    /// No public artifact command or arbitrary external reference reaches this seam.
    pub(crate) fn read_index_source(
        &self,
        read: &dyn ReadSnapshot,
        expected: &StoredGraphSourceCapture,
        max: u64,
    ) -> Result<Vec<u8>, super::lifecycle::ExactReadRefusal> {
        use super::lifecycle::ExactReadRefusal;
        let length =
            u64::try_from(expected.byte_count).map_err(|_| ExactReadRefusal::Unreadable)?;
        if length > max {
            return Err(ExactReadRefusal::TooLarge { bytes: length, max });
        }
        let _hold = self.readers.acquire(&expected.blob_digest);
        let current = read
            .graph()
            .graph_source_capture(&expected.id)
            .map_err(ExactReadRefusal::Storage)?;
        if current.as_ref() != Some(expected) {
            return Err(ExactReadRefusal::NotFound);
        }
        let blob = read
            .artifacts()
            .blob(&expected.blob_digest)
            .map_err(ExactReadRefusal::Storage)?;
        if !blob.is_some_and(|b| {
            b.generation == expected.blob_generation && b.byte_count == expected.byte_count
        }) {
            return Err(ExactReadRefusal::Unreadable);
        }
        let mut stream = self
            .files
            .open(&expected.blob_digest, &expected.blob_generation, length)
            .map_err(|_| ExactReadRefusal::Unreadable)?;
        let mut bytes = Vec::with_capacity(length as usize);
        stream
            .by_ref()
            .take(max.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| ExactReadRefusal::Unreadable)?;
        if bytes.len() as u64 != length || sha256_hex(&bytes) != expected.blob_digest {
            return Err(ExactReadRefusal::Unreadable);
        }
        Ok(bytes)
    }
}
