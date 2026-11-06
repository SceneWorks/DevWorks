use super::resolve::{candidate_space, current_candidate};
use super::{
    PreparedEmbeddingProbe, ProbeOutcome, ProbePreparation, ResolvedEmbeddingConfiguration,
    resolve_selected_read,
};
use crate::context::{InvocationContext, PrincipalKind};
use crate::ports::storage::*;
use crate::protocol::OperationState;
use std::sync::Arc;

pub const EFFECT_CONSUMER: &str = "embedding_qualification";
pub const PROBE_KIND: &str = "EmbeddingQualificationV1";
/// The probe is host-owned, fixed and never contains a user's document or conversation.
pub const PROBE_TEXT: &str = "DevWorks embedding space qualification.";

pub struct EmbeddingSpaceService {
    pub(super) store: Arc<dyn ApplicationStore>,
}
impl EmbeddingSpaceService {
    pub fn new(store: Arc<dyn ApplicationStore>) -> Self {
        Self { store }
    }
    pub fn resolve_selected(
        &self,
        ctx: &InvocationContext,
    ) -> Result<ResolvedEmbeddingConfiguration, StorageError> {
        read_snapshot(self.store.as_ref(), &ReadOptions::default(), |read| {
            resolve_selected_read(read, ctx)
        })
    }

    /// Resolves only a previously persisted host intent. It never creates an inference or
    /// changes a probe's request identity. Observation of an unknown attempt uses this same fact.
    pub fn prepare_probe(
        &self,
        operation: &OperationRecord,
    ) -> Result<ProbePreparation, StorageError> {
        let id = operation.id.clone();
        run_write(self.store.as_ref(), WriteOptions::default(), move |tx| {
            let operation = tx
                .operations()
                .operation(&id)?
                .ok_or(StorageError::ScopeDenied)?;
            if operation.capability != "router.configure"
                || operation.schema_version != 1
                || operation.payload["kind"] != PROBE_KIND
            {
                return Err(StorageError::ScopeDenied);
            }
            let candidate_id = serde_json::from_value(operation.payload["candidateId"].clone())
                .map_err(|_| StorageError::InvalidRecord("embedding probe candidate".into()))?;
            let q = tx
                .router()
                .embedding_qualification(&candidate_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let candidate = tx
                .router()
                .embedding_candidate(&candidate_id)?
                .ok_or(StorageError::ScopeDenied)?;
            let probe = q.probe.ok_or(StorageError::ScopeDenied)?;
            if probe.operation_id != operation.id
                || probe.store_id != tx.store().store_id
                || probe.restore_epoch != tx.store().restore_epoch
            {
                return Err(StorageError::ScopeDenied);
            }
            match operation.state {
                OperationState::ObservedComplete => {
                    return Ok(ProbePreparation::Terminal(
                        if q.state == EmbeddingQualificationState::Qualified {
                            ProbeOutcome::Completed
                        } else {
                            ProbeOutcome::Failed(
                                "Qualification completed without a verified space.".into(),
                            )
                        },
                    ));
                }
                OperationState::Failed => {
                    return Ok(ProbePreparation::Terminal(ProbeOutcome::Failed(
                        q.reason
                            .unwrap_or_else(|| "The qualification failed.".into()),
                    )));
                }
                OperationState::CancelledBeforeDispatch => {
                    return Ok(ProbePreparation::Terminal(ProbeOutcome::Cancelled(
                        "The qualification was cancelled before dispatch.".into(),
                    )));
                }
                OperationState::Dispatching
                | OperationState::OutcomeUnknown
                | OperationState::NeedsInput => {}
                _ => {
                    return Ok(ProbePreparation::Waiting(
                        "The durable qualification has not dispatched.".into(),
                    ));
                }
            }
            let operation_fence = operation
                .latest_attempt()
                .ok_or(StorageError::ScopeDenied)?
                .fence();
            let context = crate::services::approvals::restore_source_context(
                tx.store(),
                &probe.actor_id,
                PrincipalKind::Human,
                &probe.source_context,
            )?;
            if tx.actors().actor_kind(&probe.actor_id)? != Some(PrincipalKind::Human)
                || !crate::authority::service::fences_hold(tx, &probe.authority_fences)?
            {
                return Err(StorageError::ScopeDenied);
            }
            let delivery = tx
                .queue()
                .entry(&probe.delivery_id)?
                .ok_or(StorageError::ScopeDenied)?;
            if delivery.consumer != EFFECT_CONSUMER
                || delivery.operation_id.as_ref() != Some(&operation.id)
            {
                return Err(StorageError::ScopeDenied);
            }
            if delivery.cancel_requested && operation.state == OperationState::Dispatching {
                return Ok(ProbePreparation::Terminal(ProbeOutcome::Cancelled(
                    "The qualification was cancelled.".into(),
                )));
            }
            let connection = tx.router().connection(&candidate.connection_id)?;
            if let Some(reason) = current_candidate(&candidate, connection.as_ref())? {
                return Ok(ProbePreparation::Terminal(
                    if operation.state == OperationState::Dispatching {
                        ProbeOutcome::Failed(reason.into())
                    } else {
                        ProbeOutcome::Unknown(reason.into())
                    },
                ));
            }
            let connection = connection.ok_or(StorageError::ScopeDenied)?;
            let space = candidate_space(&candidate)?;
            Ok(ProbePreparation::Ready(Box::new(PreparedEmbeddingProbe {
                candidate,
                probe,
                connection,
                space,
                context,
                operation_fence,
                operation_state: operation.state,
            })))
        })
    }
}
