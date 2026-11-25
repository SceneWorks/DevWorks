//! Reconcile request completion from the persisted review, never by dispatching a second reviewer.
use super::{ReviewService, ReviewStateRecord, decode};
use crate::ports::storage::{OperationRecord, ReadOptions, StorageError, read_snapshot};
use crate::protocol::Id;
use crate::protocol::records::ReviewState;
use crate::protocol::results::OperationResult;
use crate::services::effect::EffectOutcome;
use crate::services::kernel::Failure;
use serde_json::json;

impl ReviewService {
    /// Returns None while the independently scheduled review is pending. A complete record is
    /// the checkpoint even if the process stopped before receiving the dispatch acknowledgment.
    pub fn observe_request(
        &self,
        operation: &OperationRecord,
    ) -> Result<Option<EffectOutcome>, StorageError> {
        if operation.capability != "reviews.request" || operation.schema_version != 1 {
            return Err(StorageError::ScopeDenied);
        }
        let id: Id = serde_json::from_value(operation.payload["reviewId"].clone())
            .map_err(|_| StorageError::ScopeDenied)?;
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), |snapshot| {
            let row = snapshot
                .git()
                .code_review(&id)?
                .ok_or(StorageError::ScopeDenied)?;
            let saved: ReviewStateRecord = decode(&row.body).map_err(Failure::into_storage)?;
            if saved.operation != operation.id || operation.root_id.as_ref() != Some(&row.root_id) {
                return Err(StorageError::ScopeDenied);
            }
            Ok(match saved.value.state {
                ReviewState::Pending | ReviewState::Running => None,
                ReviewState::Ready | ReviewState::ChangesRequired => {
                    let result = OperationResult::ReviewsRequest { value: saved.value };
                    Some(EffectOutcome::Completed(json!({"result":result})))
                }
                ReviewState::Blocked => Some(EffectOutcome::Failed(
                    "the saved review did not complete".into(),
                )),
            })
        })
    }
}
