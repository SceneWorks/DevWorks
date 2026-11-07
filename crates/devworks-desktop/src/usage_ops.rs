//! Typed usage and tariff gateway frames; no price, coverage or budget decisions live here.
use crate::{
    backend::Backend,
    ops::{self, Failure},
};
use devworks_core::{
    gateway::{CommandOutcome, QueryOutcome},
    protocol::{
        CommandReply, OperationState,
        records::*,
        results::{DomainDecision, OperationResult},
    },
};
use serde_json::Value;
pub fn query(b: &dyn Backend, op: &str, body: Value) -> Vec<u8> {
    ops::at_revision(ops::query_frame(b, op, body), 3)
}
pub fn command(b: &dyn Backend, op: &str, body: Value) -> Vec<u8> {
    ops::at_revision(ops::command_frame(b, op, body), 3)
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
