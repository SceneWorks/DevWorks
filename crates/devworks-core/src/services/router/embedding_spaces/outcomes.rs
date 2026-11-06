use super::{EmbeddingSpaceService, PROBE_KIND, ProbeOutcome};
use crate::ports::IdKind;
use crate::ports::storage::*;
use crate::protocol::{EventId, Id, OperationState};
use serde_json::json;

/// One bounded metadata reconciliation page. The runtime yields/checks shutdown between pages.
pub struct ProbeOutcomePage {
    pub next: Option<Id>,
    pub updated: usize,
}
fn outcome(operation: &OperationRecord, cancelled: bool) -> Option<ProbeOutcome> {
    let reason = operation
        .latest_attempt()
        .into_iter()
        .flat_map(|a| a.observations.iter().rev())
        .find_map(|o| {
            o.pointer("/effect/cause")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "The qualification operation settled without a positive result.".into());
    match operation.state {
        OperationState::Failed if cancelled => Some(ProbeOutcome::Cancelled(reason)),
        OperationState::Failed => Some(ProbeOutcome::Failed(reason)),
        OperationState::CancelledBeforeDispatch => Some(ProbeOutcome::Cancelled(
            "The qualification was cancelled before dispatch.".into(),
        )),
        OperationState::OutcomeUnknown | OperationState::NeedsInput => {
            Some(ProbeOutcome::Unknown(reason))
        }
        _ => None,
    }
}
impl EmbeddingSpaceService {
    /// Called after the dispatcher has durably settled the outer operation. Before transaction
    /// B this returns false; it never rewrites the journal or anticipates a future outcome.
    pub fn record_probe_outcome(
        &self,
        supplied: &OperationRecord,
        outcome: ProbeOutcome,
    ) -> Result<bool, StorageError> {
        let id = supplied.id.clone();
        let fence = supplied.latest_attempt().map(AttemptRecord::fence);
        run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
            let operation = tx
                .operations()
                .operation(&id)?
                .ok_or(StorageError::ScopeDenied)?;
            if operation.capability != "router.configure" || operation.payload["kind"] != PROBE_KIND
            {
                return Err(StorageError::ScopeDenied);
            }
            if operation.latest_attempt().map(AttemptRecord::fence) != fence {
                return Ok(false);
            }
            if fence.is_none() && operation.state != OperationState::CancelledBeforeDispatch {
                return Ok(false);
            }
            let candidate_id: Id = serde_json::from_value(operation.payload["candidateId"].clone())
                .map_err(|_| StorageError::InvalidRecord("embedding probe candidate".into()))?;
            let q = tx
                .router()
                .embedding_qualification(&candidate_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let Some(probe) = q.probe.as_ref().filter(|p| p.operation_id == operation.id) else {
                return Ok(false);
            };
            let cancelled = tx
                .queue()
                .entry(&probe.delivery_id)?
                .is_some_and(|e| e.cancel_requested || e.state == QueueState::Cancelled);
            let (state, reason, settled) = match outcome {
                ProbeOutcome::Failed(reason) => (
                    EmbeddingQualificationState::Failed,
                    reason,
                    operation.state == OperationState::Failed && !cancelled,
                ),
                ProbeOutcome::Unknown(reason) => (
                    EmbeddingQualificationState::Unknown,
                    reason,
                    matches!(
                        operation.state,
                        OperationState::OutcomeUnknown | OperationState::NeedsInput
                    ),
                ),
                ProbeOutcome::Cancelled(reason) => (
                    EmbeddingQualificationState::Cancelled,
                    reason,
                    operation.state == OperationState::CancelledBeforeDispatch
                        || (cancelled
                            && matches!(
                                operation.state,
                                OperationState::ObservedComplete | OperationState::Failed
                            )),
                ),
                ProbeOutcome::Completed => {
                    return Err(StorageError::InvalidRecord(
                        "positive probe completion requires staged native evidence".into(),
                    ));
                }
            };
            if !settled || q.state == state {
                return Ok(false);
            }
            let saved = tx.router().finish_embedding_qualification(
                &candidate_id,
                q.revision,
                &operation.id,
                fence.as_ref(),
                state,
                Some(&reason),
            )?;
            let event = EventId(tx.new_id(IdKind::Event));
            tx.history().append(NewEvent {
                id: event,
                actor: probe.actor_id.clone(),
                project_id: None,
                root_id: None,
                operation_id: Some(operation.id),
                aggregate_kind: "embedding_space".into(),
                aggregate_id: candidate_id.as_str().into(),
                aggregate_revision: saved.revision,
                kind: "EmbeddingQualificationSettled".into(),
                payload: json!({"candidateId":candidate_id,"state":state,"reason":reason}),
            })?;
            Ok(true)
        })
    }

    /// Repairs negative metadata after a lost settlement acknowledgement or restart. It observes
    /// only the original journal and never creates/retries a native inference.
    pub fn reconcile_probe_outcomes(
        &self,
        after: Option<&Id>,
    ) -> Result<ProbeOutcomePage, StorageError> {
        let (next, records) =
            read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
                let candidates = read.router().embedding_candidates(after, 128)?;
                let next = if candidates.len() == 128 {
                    candidates.last().map(|c| c.id.clone())
                } else {
                    None
                };
                let mut records = Vec::new();
                for candidate in candidates {
                    let Some(q) = read.router().embedding_qualification(&candidate.id)? else {
                        continue;
                    };
                    if !matches!(
                        q.state,
                        EmbeddingQualificationState::Qualifying
                            | EmbeddingQualificationState::Unknown
                    ) {
                        continue;
                    }
                    let Some(probe) = q.probe else {
                        continue;
                    };
                    let Some(operation) = read.operations().operation(&probe.operation_id)? else {
                        continue;
                    };
                    let cancelled = read
                        .queue()
                        .entry(&probe.delivery_id)?
                        .is_some_and(|e| e.cancel_requested || e.state == QueueState::Cancelled);
                    if let Some(result) = outcome(&operation, cancelled) {
                        records.push((operation, result));
                    }
                }
                Ok((next, records))
            })?;
        let mut updated = 0;
        for (operation, result) in records {
            if self.record_probe_outcome(&operation, result)? {
                updated += 1;
            }
        }
        Ok(ProbeOutcomePage { next, updated })
    }
}
