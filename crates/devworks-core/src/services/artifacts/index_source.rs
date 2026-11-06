//! Private captured index bytes. These are not public artifacts or artifact versions.
use super::{ArtifactService, StagedFile, put_staged_blob};
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
}

impl ArtifactService {
    /// Verified bounded-buffer access for graph source pages. The visitor may retain only its
    /// requested byte window; a successful return has verified the entire immutable blob.
    pub(crate) fn scan_index_source(
        &self,
        read: &dyn ReadSnapshot,
        expected: &StoredGraphSourceCapture,
        mut visit: impl FnMut(&[u8]) -> bool,
    ) -> Result<(), super::lifecycle::ExactReadRefusal> {
        use super::lifecycle::ExactReadRefusal;
        use sha2::{Digest, Sha256};
        let length =
            u64::try_from(expected.byte_count).map_err(|_| ExactReadRefusal::Unreadable)?;
        let _hold = self.readers.acquire(&expected.blob_digest);
        if read
            .graph()
            .graph_source_capture(&expected.id)
            .map_err(ExactReadRefusal::Storage)?
            .as_ref()
            != Some(expected)
        {
            return Err(ExactReadRefusal::NotFound);
        }
        if !read
            .artifacts()
            .blob(&expected.blob_digest)
            .map_err(ExactReadRefusal::Storage)?
            .is_some_and(|b| {
                b.generation == expected.blob_generation && b.byte_count == expected.byte_count
            })
        {
            return Err(ExactReadRefusal::Unreadable);
        }
        let mut stream = self
            .files
            .open_controlled(
                &expected.blob_digest,
                &expected.blob_generation,
                length,
                &mut || !visit(&[]),
            )
            .map_err(|_| ExactReadRefusal::Unreadable)?;
        let mut buffer = [0; 8192];
        let mut hash = Sha256::new();
        let mut total = 0u64;
        loop {
            let count = stream
                .read(&mut buffer)
                .map_err(|_| ExactReadRefusal::Unreadable)?;
            if count == 0 {
                break;
            }
            total = total
                .checked_add(count as u64)
                .ok_or(ExactReadRefusal::Unreadable)?;
            if total > length {
                return Err(ExactReadRefusal::Unreadable);
            }
            hash.update(&buffer[..count]);
            if !visit(&buffer[..count]) {
                return Err(ExactReadRefusal::Unreadable);
            }
        }
        if total != length
            || hash
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
                != expected.blob_digest
        {
            return Err(ExactReadRefusal::Unreadable);
        }
        Ok(())
    }
}
