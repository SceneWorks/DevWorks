//! Typed usage and tariff gateway frames; no price, coverage or budget decisions live here.
use crate::{
    backend::{Backend, screen_slot},
    frame::{self, Negotiate},
    ops::{self, Failure},
};

/// The Usage requests the screen waits for. The names are persisted (see [`crate::backend::Slot`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum UsageSlot {
    Usage(u64, bool),
    UsageOptions(u64),
    Prices(u64, bool),
    PriceProbe(u64),
    PriceOverride,
    PriceRefresh,
    PriceOperation,
}
screen_slot!(UsageSlot, Usage);
use devworks_core::{
    gateway::{CommandOutcome, QueryOutcome},
    operations::{
        OperationInput as In, history,
        router::{self, PriceOverrideInput, PriceRefreshInput, PriceStatusInput},
    },
    protocol::{
        CommandReply, Id, OperationId, OperationState, Query, QueryRequest, Revision,
        records::*,
        results::{DomainDecision, OperationResult},
        support::{BoundedList, Items},
    },
};
/// The frame of a router usage or price request (at the newest revision). A request that did not
/// convert (`None`) is no bytes, which the gateway rejects like any malformed request.
pub fn query(b: &dyn Backend, input: impl Into<Option<In>>) -> Vec<u8> {
    frame::build(b, Negotiate::Latest, input.into())
}
pub fn command(b: &dyn Backend, input: impl Into<Option<In>>) -> Vec<u8> {
    frame::build(b, Negotiate::Latest, input.into())
}
fn ids(values: &[&str]) -> Option<Items<Id>> {
    frame::bounded(
        values
            .iter()
            .map(|v| frame::id(v))
            .collect::<Option<Vec<_>>>()?,
    )
}
/// What a `router.price_status` read asks for: the models, and the providers and snapshots when
/// given, with or without history.
pub fn price_status(
    models: &[&str],
    providers: Option<&[&str]>,
    snapshots: Option<&[&str]>,
    history: bool,
) -> Option<PriceStatusInput> {
    Some(PriceStatusInput {
        model_ids: ids(models)?,
        page: frame::page(100, None)?,
        provider_ids: frame::lift(providers.map(ids))?,
        include_history: Some(history),
        snapshot_ids: frame::lift(snapshots.map(ids))?,
    })
}
/// A one-row `router.price_status` read, to see whether the catalog moved.
pub fn price_probe() -> Option<In> {
    Some(In::Router(router::Input::PriceStatus(PriceStatusInput {
        model_ids: BoundedList(Vec::new()),
        page: frame::page(1, None)?,
        provider_ids: None,
        include_history: None,
        snapshot_ids: None,
    })))
}
/// `price` as a request, continuing after `cursor` when given.
pub fn price_page(mut price: PriceStatusInput, cursor: Option<&str>) -> Option<In> {
    if let Some(cursor) = cursor {
        price.page.cursor = Some(frame::text(cursor)?);
    }
    Some(In::Router(router::Input::PriceStatus(price)))
}
/// `router.price_override` at the record's `revision`: the tariffs to set, or clearing the
/// override when there are none.
pub fn price_override(
    model: &str,
    revision: &str,
    tariffs: Option<Vec<PriceTariff>>,
) -> Option<In> {
    Some(In::Router(router::Input::PriceOverride(
        PriceOverrideInput {
            model_id: frame::id(model)?,
            expected_revision: frame::text(revision)?,
            price: None,
            change: Some(match tariffs {
                Some(tariffs) => PriceOverrideChange::Set {
                    tariffs: frame::bounded(tariffs)?,
                },
                None => PriceOverrideChange::Clear {},
            }),
        },
    )))
}
/// `router.price_refresh` of the official price sources, at the catalog's revision.
pub fn price_refresh(providers: &[String], revision: &Revision) -> Option<In> {
    Some(In::Router(router::Input::PriceRefresh(PriceRefreshInput {
        provider_ids: frame::bounded(
            providers
                .iter()
                .map(|p| frame::id(p))
                .collect::<Option<Vec<_>>>()?,
        )?,
        expected_price_revision: revision.clone(),
    })))
}
/// `history.operation_status` of an operation.
pub fn operation_status(b: &dyn Backend, operation: &OperationId) -> Vec<u8> {
    frame::with(b, Negotiate::Initial, || {
        Some(In::History(history::Input::OperationStatus(QueryRequest {
            protocol: frame::protocol(),
            query_id: frame::query_id(b)?,
            query: Query::Operation {
                operation_id: operation.clone(),
            },
        })))
    })
}
pub fn usage(q: QueryOutcome) -> Result<UsageQueryResult, Failure> {
    match ops::query_result(q)?.0 {
        OperationResult::RouterUsage {
            value: RouterUsageResult::Current(value),
        } => Ok(value),
        other => Err(ops::unexpected("router.usage revision 3", &other)),
    }
}
pub fn prices(q: QueryOutcome) -> Result<PriceCatalogView, Failure> {
    match ops::query_result(q)?.0 {
        OperationResult::RouterPriceStatus {
            value: PriceCatalogResult::Current(value),
        } => Ok(value),
        other => Err(ops::unexpected("router.price_status revision 3", &other)),
    }
}
pub fn overridden(q: CommandOutcome) -> Result<PriceOverrideOutcome, Failure> {
    match ops::command_result(q)? {
        OperationResult::RouterPriceOverride {
            value: PriceOverrideResult::Current(value),
        } => Ok(value),
        other => Err(ops::unexpected("router.price_override revision 3", &other)),
    }
}
pub fn refreshed(q: CommandOutcome) -> Result<OperationRef, Failure> {
    if let Ok(CommandReply::Recorded { receipt, .. }) = &q
        && let DomainDecision::Accepted { operation_id } = &receipt.decision
    {
        return Ok(OperationRef {
            operation_id: operation_id.clone(),
            state: OperationState::IntentRecorded,
        });
    }
    match ops::command_result(q)? {
        OperationResult::RouterPriceRefresh { value } => Ok(value),
        other => Err(ops::unexpected("router.price_refresh", &other)),
    }
}
