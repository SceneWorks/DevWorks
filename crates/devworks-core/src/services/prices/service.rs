//! Shared price maintenance admission. Public HTTP is performed only after this transaction
//! commits its durable intent; publication retains the last successful provider snapshot.
use super::{inspection, refresh};
use crate::context::{InvocationContext, PrincipalKind};
use crate::gateway::{AdmittedCommand, AdmittedQuery};
use crate::operations::{OperationInput, router as ops};
use crate::ports::storage::*;
use crate::ports::{
    Clock, CommandHandler, Decided, HandlerFailure, IdKind, IdSource, QueryAnswer, QueryHandler,
};
use crate::protocol::records::{PriceOverrideChange, PriceOverrideOutcome, PriceOverrideResult};
use crate::protocol::results::{DomainDecision, OperationResult};
use crate::protocol::{DomainErrorCode, DomainFault, EventId};
use crate::services::sessions::{Failure, Refusal, counter, decided, not_found, query_fault, rev};
use serde_json::json;
use std::sync::Arc;

pub const OPERATIONS: [&str; 3] = [
    "router.price_status",
    "router.price_refresh",
    "router.price_override",
];
pub struct PriceService {
    pub(crate) ids: Arc<dyn IdSource>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) source_available: bool,
}
impl PriceService {
    pub fn new(ids: Arc<dyn IdSource>, clock: Arc<dyn Clock>, source_available: bool) -> Self {
        Self {
            ids,
            clock,
            source_available,
        }
    }
    fn override_price(
        &self,
        tx: &mut dyn WriteTransaction,
        ctx: &InvocationContext,
        input: &ops::PriceOverrideInput,
        schema: u32,
    ) -> Result<DomainDecision, Failure> {
        if ctx.principal_kind() != PrincipalKind::Human
            || tx.actors().actor_kind(ctx.principal())? != Some(PrincipalKind::Human)
        {
            return Err(Refusal::new(
                DomainErrorCode::PermissionDenied,
                "price_override",
                Some(input.model_id.clone()),
            )
            .into());
        }
        let model = refresh::model(tx.router(), &input.model_id)?
            .ok_or_else(|| not_found("model", &input.model_id))?;
        if tx
            .router()
            .embedding_price_subject(&input.model_id)?
            .is_some_and(|subject| subject.current_candidate_id.is_none())
        {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                "embedding_price_binding",
                Some(input.model_id.clone()),
            )
            .into());
        }
        let current = tx.usage().price_override_revision(&input.model_id)?;
        if current != input.expected_revision.get() {
            return Err(refresh::conflict(
                "price_override",
                &input.model_id,
                input.expected_revision.get(),
                current,
            ));
        }
        let (mode, tariffs) = match (&input.price, &input.change) {
            (Some(price), None) => ("Set", inspection::legacy_tariffs(&model, price)),
            (None, Some(PriceOverrideChange::Set { tariffs })) => (
                "Set",
                tariffs
                    .0
                    .iter()
                    .map(inspection::published)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            (None, Some(PriceOverrideChange::Clear {})) => ("Clear", vec![]),
            _ => {
                return Err(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "price_change",
                    None,
                )
                .into());
            }
        };
        // A model-specific override cannot silently quote another upstream or endpoint.
        if tariffs.iter().any(|t| {
            t.upstream_model_id != model.upstream_model_id
                || t.endpoint_profile
                    .as_ref()
                    .is_some_and(|p| p != &model.endpoint_profile)
        }) {
            return Err(Refusal::new(
                DomainErrorCode::SourceChanged,
                "model_price_binding",
                Some(model.id),
            )
            .into());
        }
        refresh::validate_tariffs(&tariffs)?;
        let next = current.checked_add(1).ok_or_else(|| {
            StorageError::InvalidRecord("price override revision overflow".into())
        })?;
        let row = StoredPriceOverride {
            model_id: input.model_id.clone(),
            revision: next,
            mode: mode.into(),
            tariffs,
            updated_by: ctx.principal().clone(),
            updated_at_ms: tx.now_ms(),
        };
        // Construct the exact old result before any write; no projection may hide dimensions.
        let result = if schema < 3 {
            if mode != "Set" {
                return Err(Refusal::new(
                    DomainErrorCode::UnsupportedCapability,
                    "legacy_price_clear",
                    None,
                )
                .into());
            }
            PriceOverrideResult::Legacy(inspection::legacy_price(
                &model,
                &row.tariffs,
                next,
                "UserConfigured",
                row.updated_at_ms,
                None,
            )?)
        } else {
            PriceOverrideResult::Current(if mode == "Set" {
                PriceOverrideOutcome::Set {
                    model_id: row.model_id.clone(),
                    revision: rev(next),
                    updated_by: Some(row.updated_by.clone()),
                    updated_at_ms: Some(counter(row.updated_at_ms)),
                    tariffs: crate::protocol::support::BoundedList(
                        row.tariffs
                            .iter()
                            .map(inspection::tariff)
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                }
            } else {
                PriceOverrideOutcome::Cleared {
                    model_id: row.model_id.clone(),
                    revision: rev(next),
                    updated_by: Some(row.updated_by.clone()),
                    updated_at_ms: Some(counter(row.updated_at_ms)),
                }
            })
        };
        tx.usage().put_price_override(&row, current)?;
        let event = EventId(tx.new_id(IdKind::Event));
        tx.history().append(NewEvent{id:event,actor:ctx.principal().clone(),project_id:None,root_id:None,operation_id:None,aggregate_kind:"price_override".into(),aggregate_id:row.model_id.as_str().into(),aggregate_revision:row.revision,kind:if mode=="Set"{"PriceOverrideSet"}else{"PriceOverrideCleared"}.into(),payload:json!({"modelId":row.model_id,"revision":row.revision.to_string(),"mode":row.mode})})?;
        Ok(DomainDecision::Applied {
            result: OperationResult::RouterPriceOverride { value: result },
        })
    }
}
impl QueryHandler for PriceService {
    fn query(
        &self,
        ctx: &InvocationContext,
        q: &AdmittedQuery,
        s: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let result = match q.input() {
            OperationInput::Router(ops::Input::PriceStatus(input)) => {
                inspection::query(self, s, ctx, input, q.schema_revision().0)
            }
            _ => {
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into())
            }
        };
        let value = result.map_err(|e| {
            query_fault(
                e,
                "price_catalog",
                crate::protocol::CorrelationId(self.ids.next_id(IdKind::Correlation)),
            )
        })?;
        let revision = s.usage().price_revision().map_err(|e| {
            query_fault(
                e.into(),
                "price_catalog",
                crate::protocol::CorrelationId(self.ids.next_id(IdKind::Correlation)),
            )
        })?;
        Ok(QueryAnswer {
            stamp: s
                .stamp()
                .read_stamp(ctx.policy_revision().clone(), rev(revision)),
            result: OperationResult::RouterPriceStatus { value },
        })
    }
}
impl CommandHandler for PriceService {
    fn decide(
        &self,
        ctx: &InvocationContext,
        c: &AdmittedCommand,
        tx: &mut dyn WriteTransaction,
    ) -> Result<Decided, HandlerFailure> {
        match c.input() {
            OperationInput::Router(ops::Input::PriceRefresh(input)) => {
                let result = if self.source_available {
                    refresh::request(
                        tx,
                        Some(ctx),
                        input.expected_price_revision.get(),
                        &input.provider_ids.0,
                    )
                    .map(|op| Decided {
                        decision: DomainDecision::Accepted {
                            operation_id: op.clone(),
                        },
                        operation_id: Some(op),
                    })
                } else {
                    Err(Refusal::new(
                        DomainErrorCode::UnsupportedCapability,
                        "official_price_source_unavailable",
                        None,
                    )
                    .into())
                };
                match result {
                    Ok(value) => Ok(value),
                    Err(f) => decided(tx, Err(f)),
                }
            }
            OperationInput::Router(ops::Input::PriceOverride(input)) => {
                let result = self.override_price(tx, ctx, input, c.schema_revision().0);
                decided(tx, result)
            }
            _ => decided(
                tx,
                Err(Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None).into()),
            ),
        }
    }
}
