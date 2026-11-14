use super::{EmbeddingSpaceService, resolve_selected_read};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::AdmittedQuery;
use crate::operations::{OperationInput, router as ops};
use crate::ports::storage::*;
use crate::ports::{QueryAnswer, QueryHandler};
use crate::protocol::records::{
    EmbeddingDimensionMode, EmbeddingMetric, EmbeddingSpacePage, EmbeddingSpaceRecord,
    EmbeddingSpaceStatus, OperationRef,
};
use crate::protocol::results::OperationResult;
use crate::protocol::support::{BoundedText, PositiveCounter};
use crate::protocol::{CorrelationId, DomainErrorCode, DomainFault, Id, OperationState};
use crate::services::cursor::{self, CursorBinding};
use crate::services::sessions::{Failure, Refusal, counter, query_fault, rev};

fn invalid(why: &str) -> StorageError {
    StorageError::unavailable(Unavailable::Corrupt, why)
}
impl EmbeddingSpaceService {
    fn record(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        c: StoredEmbeddingCandidate,
    ) -> Result<EmbeddingSpaceRecord, StorageError> {
        let q = read
            .router()
            .embedding_qualification(&c.id)?
            .ok_or_else(|| invalid("embedding qualification absent"))?;
        let connection = read.router().connection(&c.connection_id)?;
        let source_reason = super::resolve::current_candidate(&c, connection.as_ref())?;
        let operation = q
            .probe
            .as_ref()
            .map(|p| read.operations().operation(&p.operation_id))
            .transpose()?
            .flatten();
        let mut status = match q.state {
            EmbeddingQualificationState::Unqualified => EmbeddingSpaceStatus::Unqualified,
            EmbeddingQualificationState::Qualifying => EmbeddingSpaceStatus::Qualifying,
            EmbeddingQualificationState::Qualified => EmbeddingSpaceStatus::Qualified,
            EmbeddingQualificationState::Failed => EmbeddingSpaceStatus::Failed,
            EmbeddingQualificationState::Unknown => EmbeddingSpaceStatus::Unknown,
            EmbeddingQualificationState::Cancelled => EmbeddingSpaceStatus::Cancelled,
        };
        let mut reason = q.reason.clone();
        if let Some(operation) = &operation {
            status = match operation.state {
                OperationState::IntentRecorded
                | OperationState::Preflighted
                | OperationState::Dispatching
                | OperationState::RetryPending => EmbeddingSpaceStatus::Qualifying,
                OperationState::OutcomeUnknown | OperationState::NeedsInput => {
                    EmbeddingSpaceStatus::Unknown
                }
                OperationState::Failed => EmbeddingSpaceStatus::Failed,
                OperationState::CancelledBeforeDispatch => EmbeddingSpaceStatus::Cancelled,
                OperationState::ObservedComplete => status,
            };
            if matches!(
                status,
                EmbeddingSpaceStatus::Failed | EmbeddingSpaceStatus::Unknown
            ) && reason.is_none()
            {
                reason = operation
                    .attempts
                    .iter()
                    .rev()
                    .flat_map(|a| a.observations.iter().rev())
                    .find_map(|o| {
                        o.pointer("/effect/cause")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned)
                    });
            }
        } else if q.probe.is_some() {
            status = EmbeddingSpaceStatus::Unavailable;
            reason = Some("The original qualification operation is unavailable.".into());
        }
        let cancelled = q
            .probe
            .as_ref()
            .map(|p| read.queue().entry(&p.delivery_id))
            .transpose()?
            .flatten()
            .is_some_and(|e| e.cancel_requested || e.state == QueueState::Cancelled);
        if cancelled
            && operation.as_ref().is_some_and(|op| {
                matches!(
                    op.state,
                    OperationState::Failed
                        | OperationState::ObservedComplete
                        | OperationState::CancelledBeforeDispatch
                )
            })
        {
            status = EmbeddingSpaceStatus::Cancelled;
            reason =
                Some("The qualification was cancelled. Any native usage remains recorded.".into());
        } else if cancelled
            && operation
                .as_ref()
                .is_some_and(|op| op.state == OperationState::Dispatching)
        {
            reason = Some("Cancellation requested; waiting for the attempt to close.".into());
        } else if operation
            .as_ref()
            .is_some_and(|op| op.state == OperationState::ObservedComplete)
            && matches!(
                q.state,
                EmbeddingQualificationState::Qualifying | EmbeddingQualificationState::Unknown
            )
        {
            status = EmbeddingSpaceStatus::Unavailable;
            reason = Some("The original operation has no published positive qualification.".into());
        }
        if matches!(status, EmbeddingSpaceStatus::Qualified)
            && (!operation
                .as_ref()
                .is_some_and(|o| o.state == OperationState::ObservedComplete)
                || !super::resolve::qualification_current_read(read, Some(&q))?)
        {
            status = EmbeddingSpaceStatus::Unavailable;
            reason=Some("The positive qualification evidence or its original authority is no longer current. Qualify again before use.".into());
        }
        if let Some(source_reason) = source_reason {
            let identity_changed = connection.as_ref().is_some_and(|s| {
                s.profile != "Embedding"
                    || s.account_revision != c.account_revision
                    || s.account_digest != c.account_digest
                    || s.endpoint != c.endpoint
            });
            status = if identity_changed {
                EmbeddingSpaceStatus::Stale
            } else {
                EmbeddingSpaceStatus::Unavailable
            };
            reason = Some(source_reason.into());
        }
        let can_qualify = self.qualification_available()
            && ctx.principal_kind() == PrincipalKind::Human
            && source_reason.is_none()
            && operation.as_ref().is_none_or(|o| is_terminal(o.state))
            && !matches!(status, EmbeddingSpaceStatus::Qualified);
        let evidence = q.evidence.as_ref();
        let pricing_model_id = read
            .router()
            .embedding_price_subject_for_candidate(&c.id)?
            .map(|subject| subject.model_id);
        Ok(EmbeddingSpaceRecord {
            id: c.id,
            space_id: c.space_id,
            pricing_model_id,
            revision: rev(q.revision),
            name: BoundedText(c.name),
            connection_id: c.connection_id,
            account_revision: rev(c.account_revision),
            upstream_model_id: BoundedText(c.upstream_model_id),
            dimensions: PositiveCounter(c.dimensions.to_string()),
            dimension_mode: match c.dimension_mode.as_str() {
                "Native" => EmbeddingDimensionMode::Native,
                "Parameter" => EmbeddingDimensionMode::Parameter,
                _ => return Err(invalid("embedding dimension mode")),
            },
            metric: match c.metric.as_str() {
                "L2" => EmbeddingMetric::L2,
                "Cosine" => EmbeddingMetric::Cosine,
                _ => return Err(invalid("embedding metric")),
            },
            model_digest: crate::protocol::provider::Digest(c.model_digest),
            configuration_digest: crate::protocol::provider::Digest(c.configuration_digest),
            status,
            can_qualify,
            reason: reason.map(BoundedText),
            qualification_operation: operation.map(|o| OperationRef {
                operation_id: o.id,
                state: o.state,
            }),
            qualification_request_id: q.probe.as_ref().map(|p| p.request_id.clone()),
            qualification_attempt_id: evidence.map(|e| e.attempt_id.clone()),
            result_artifact: evidence.map(|e| e.result_artifact.clone()),
            created_at_ms: counter(c.created_at_ms),
            updated_at_ms: counter(q.updated_at_ms),
        })
    }
    fn page(
        &self,
        read: &dyn ReadSnapshot,
        ctx: &InvocationContext,
        input: &ops::EmbeddingSpacesInput,
    ) -> Result<OperationResult, Failure> {
        let selected = read.router().embedding_selection()?;
        let source = resolve_selected_read(read, ctx)?;
        let inventory = read.router().embedding_inventory_revision()?;
        let boot = self
            .store
            .info()
            .boot_id
            .map(|id| id.as_str().to_owned())
            .unwrap_or_default();
        let native = if self.qualification_available() {
            "Native"
        } else {
            "Unavailable"
        };
        let binding = CursorBinding::new(
            ctx,
            "router.embedding_spaces",
            &[source.source_digest(), &boot, native],
            inventory,
        );
        let rows = cursor::page(
            &binding,
            &input.page,
            |after, n| {
                let after = after
                    .map(|a| Id::new(a).map_err(|_| StorageError::CursorExpired))
                    .transpose()?;
                read.router().embedding_candidates(after.as_ref(), n)
            },
            |c| c.id.as_str().to_owned(),
        )?;
        let page = rows.try_into_page(|c| self.record(read, ctx, c))?;
        let available = self.qualification_available();
        Ok(OperationResult::RouterEmbeddingSpaces {
            value: EmbeddingSpacePage {
                items: page.items,
                next_cursor: page.next_cursor,
                has_more: page.has_more,
                selection_revision: rev(selected.revision),
                selected_candidate_id: selected.candidate_id,
                selected_space_id: selected.space_id,
                qualification_available: available,
                availability_reason: (!available).then(|| {
                    BoundedText(
                        "Native embedding qualification is unavailable in this runtime.".into(),
                    )
                }),
            },
        })
    }
}
impl QueryHandler for EmbeddingSpaceService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        read: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let result = match query.input() {
            OperationInput::Router(ops::Input::EmbeddingSpaces(input)) => {
                self.page(read, ctx, input)
            }
            _ => Err(Refusal::new(
                DomainErrorCode::UnsupportedCapability,
                "embedding_spaces",
                None,
            )
            .into()),
        }
        .map_err(|f| {
            query_fault(
                f,
                "embedding_spaces",
                CorrelationId(ctx.invocation_id().clone()),
            )
        })?;
        Ok(QueryAnswer {
            stamp: read.stamp().read_stamp(
                ctx.policy_revision().clone(),
                rev(read
                    .router()
                    .embedding_selection()
                    .map_err(|e| {
                        query_fault(
                            e.into(),
                            "embedding_spaces",
                            CorrelationId(ctx.invocation_id().clone()),
                        )
                    })?
                    .revision),
            ),
            result,
        })
    }
}
