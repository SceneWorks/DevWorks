//! Attempt accounting, immutable price attribution and feature-only spending admission.
pub mod accounting;
pub mod budget;
pub mod host;
pub mod ingest;
pub mod meter;
pub mod money;
pub mod pricing;
pub mod reports;

use crate::{
    context::InvocationContext,
    gateway::AdmittedQuery,
    operations::{OperationInput, router as ops},
    ports::{Clock, IdKind, IdSource, QueryAnswer, QueryHandler, storage::ReadSnapshot},
    protocol::{
        CorrelationId, DomainErrorCode, DomainFault, Revision,
        records::{
            PriceOrigin, RouterUsageResult, UsageCostKind, UsageItem, UsagePriceKind,
            UsageQueryResult, UsageReport,
        },
        results::OperationResult,
        support::{BoundedList, Money},
    },
    services::sessions::{Failure, Refusal, query_fault},
};
use std::sync::Arc;

/// The same authorized accounting query is registered for native UI and agent tools.
pub struct UsageService {
    ids: Arc<dyn IdSource>,
    clock: Arc<dyn Clock>,
}
impl UsageService {
    pub fn new(ids: Arc<dyn IdSource>, clock: Arc<dyn Clock>) -> Self {
        Self { ids, clock }
    }
}
fn unsupported_legacy() -> Failure {
    Refusal::new(DomainErrorCode::ViewUnavailable, "usage_revision", None).into()
}
fn dollars(micro: i64) -> Money {
    Money(format!("{}.{:06}", micro / 1_000_000, micro % 1_000_000))
}
fn legacy(current: UsageQueryResult) -> Result<UsageReport, Failure> {
    let UsageQueryResult::Attempts { page } = current else {
        return Err(unsupported_legacy());
    };
    if page.totals.legacy_usage_facts.get() > 0 {
        return Err(unsupported_legacy());
    }
    let mut items = Vec::with_capacity(page.items.0.len());
    for row in page.items.0 {
        if matches!(row.cost.price_kind, UsageCostKind::Unknown)
            || (!row.cost.monetary_complete && row.cost.amount_micro_usd.is_some())
            || row.unknown_tail
        {
            return Err(unsupported_legacy());
        }
        let price_kind = match row.cost.price_kind {
            UsageCostKind::Reported => UsagePriceKind::Reported,
            UsageCostKind::Estimated => UsagePriceKind::Estimated,
            UsageCostKind::Free => UsagePriceKind::Free,
            UsageCostKind::Untracked => UsagePriceKind::Untracked,
            UsageCostKind::Unknown => return Err(unsupported_legacy()),
        };
        let price_origin = match row
            .cost
            .classification
            .as_ref()
            .map(|v| v.0.as_str())
            .unwrap_or("Untracked")
        {
            "ReportedCharge" => PriceOrigin::ProviderReported,
            "ConfiguredEstimate" => PriceOrigin::UserConfigured,
            "OfficialEstimate" | "SubscriptionApiEquivalent" => PriceOrigin::OfficialSnapshot,
            "LocalFree" => PriceOrigin::LocalFree,
            _ => PriceOrigin::Untracked,
        };
        items.push(UsageItem {
            request_id: row.request_id.ok_or_else(unsupported_legacy)?,
            purpose: row.purpose,
            model_id: row.model_id.ok_or_else(unsupported_legacy)?,
            input_tokens: row.input_tokens.ok_or_else(unsupported_legacy)?,
            output_tokens: row.output_tokens.ok_or_else(unsupported_legacy)?,
            cache_read_tokens: row.cache_read_tokens.ok_or_else(unsupported_legacy)?,
            cache_write_tokens: row.cache_write_tokens.ok_or_else(unsupported_legacy)?,
            amount_usd: row.cost.amount_micro_usd.map(|v| dollars(v.get())),
            price_kind,
            price_origin,
            price_revision: row.cost.price_revision,
            incomplete: row.cost.incomplete,
        });
    }
    Ok(UsageReport {
        items: BoundedList(items),
        total_tracked_usd: dollars(page.totals.tracked_micro_usd.get()),
        untracked_requests: page.totals.untracked_attempts,
        incomplete_requests: page.totals.incomplete_attempts,
        next_cursor: page.next_cursor,
    })
}
impl QueryHandler for UsageService {
    fn query(
        &self,
        ctx: &InvocationContext,
        query: &AdmittedQuery,
        read: &dyn ReadSnapshot,
    ) -> Result<QueryAnswer, DomainFault> {
        let correlation = CorrelationId(self.ids.next_id(IdKind::Correlation));
        let OperationInput::Router(ops::Input::Usage(input)) = query.input() else {
            return Err(
                Refusal::new(DomainErrorCode::UnsupportedCapability, "operation", None)
                    .fault(correlation),
            );
        };
        // Revision one has no time selection; the shared report pipeline preserves its
        // complete stored selection without applying revision three's public date range.
        let selected = if query.schema_revision().0 >= 3 {
            reports::query(read, ctx, input, self.clock.now_ms().get())
        } else {
            reports::query_legacy_attempts(read, ctx, input, self.clock.now_ms().get())
        };
        let answered = selected
            .and_then(|report| {
                if query.schema_revision().0 >= 3 {
                    Ok(RouterUsageResult::Current(report))
                } else if read.usage().usage_bootstrap()?.phase != "Complete" {
                    Err(unsupported_legacy())
                } else {
                    legacy(report).map(RouterUsageResult::Legacy)
                }
            })
            .map_err(|failure| query_fault(failure, "usage", correlation))?;
        Ok(QueryAnswer {
            stamp: read.stamp().read_stamp(
                ctx.policy_revision().clone(),
                Revision::new(1).expect("positive"),
            ),
            result: OperationResult::RouterUsage { value: answered },
        })
    }
}

pub mod dimensions;
