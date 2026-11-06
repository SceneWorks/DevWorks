//! Actual current source-bound index metadata; a draft or absent head is never a ready index.
use super::IndexService;
use crate::{
    ports::storage::*,
    protocol::{
        Counter, Revision,
        records::{ConventionIndexStatus, OperationRef},
    },
    services::conventions::model,
};
use serde::de::DeserializeOwned;
fn parse<T: DeserializeOwned>(value: serde_json::Value) -> Result<T, StorageError> {
    serde_json::from_value(value)
        .map_err(|e| StorageError::InvalidRecord(format!("convention index metadata: {e}")))
}
impl IndexService {
    pub fn status(
        &self,
        read: &dyn ReadSnapshot,
        owner: &StoredConvention,
    ) -> Result<ConventionIndexStatus, StorageError> {
        let Some(index) = read
            .conventions()
            .convention_index_status(&owner.id, None)?
        else {
            return Ok(model::unindexed());
        };
        let count = read
            .conventions()
            .convention_chunk_count(&index.generation_id)?;
        let operation = read
            .operations()
            .operation(&index.operation_id)?
            .ok_or_else(|| StorageError::InvalidRecord("index operation missing".into()))?;
        Ok(ConventionIndexStatus {
            status: parse(serde_json::json!(index.status))?,
            generation_id: Some(index.generation_id),
            source_revision: Some(
                Revision::new(index.convention_revision)
                    .map_err(|e| StorageError::InvalidRecord(e.into()))?,
            ),
            source_digest: Some(parse(serde_json::json!(index.source_digest))?),
            embedding_space: index
                .embedding_space
                .map(|s| parse(serde_json::json!(s)))
                .transpose()?,
            indexed_chunks: Some(
                Counter::new(
                    i64::try_from(count)
                        .map_err(|_| StorageError::InvalidRecord("index count overflow".into()))?,
                )
                .map_err(|e| StorageError::InvalidRecord(e.into()))?,
            ),
            operation: Some(OperationRef {
                operation_id: operation.id,
                state: operation.state,
            }),
            reason: index
                .error
                .map(|e| parse(serde_json::json!(e)))
                .transpose()?,
        })
    }
}
