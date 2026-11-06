//! Actual response dimensions qualify the original immutable estimate; they never reprice it.
use super::{accounting, budget};
use crate::{
    ports::{storage::*, usage::CostClassification},
    services::runs::ControlService,
};
use serde_json::json;
#[derive(Clone, Debug)]
pub struct ObservedDimensions {
    pub service_tier: Option<String>,
    pub processing_region: Option<String>,
}
fn tier(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "default" | "standard" => "standard".into(),
        other => other.into(),
    }
}
fn region(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}
fn bounded(value: Option<String>) -> Option<String> {
    value
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.chars().take(256).collect())
}
pub(crate) fn observe(
    tx: &mut dyn WriteTransaction,
    control: &ControlService,
    attempt: &StoredInferenceAttempt,
    dimensions: ObservedDimensions,
) -> Result<(), StorageError> {
    let current = tx
        .usage()
        .inference_attempt(&attempt.attempt_id)?
        .ok_or(StorageError::ScopeDenied)?;
    let bound = accounting::snapshot(&current)?;
    if current.state != "Dispatched"
        || current.pricing_snapshot != attempt.pricing_snapshot
        || Some(&bound.admitted_boot_id) != tx.store().boot_id.as_ref()
    {
        return Err(StorageError::ScopeDenied);
    }
    let before = tx
        .usage()
        .attempt_balance(&attempt.attempt_id)?
        .ok_or(StorageError::ScopeDenied)?;
    let mut summary = accounting::summary(&before)?;
    let actual_tier = bounded(dimensions.service_tier);
    let actual_region = bounded(dimensions.processing_region);
    if actual_tier.is_none() && actual_region.is_none() {
        return Ok(());
    }
    let same_tier = actual_tier
        .as_ref()
        .is_none_or(|v| Some(v) == summary.reported_service_tier.as_ref());
    let same_region = actual_region
        .as_ref()
        .is_none_or(|v| Some(v) == summary.reported_processing_region.as_ref());
    if same_tier && same_region {
        return Ok(());
    }
    if let Some(actual) = &actual_tier {
        if tier(actual) != tier(bound.query.service_tier.as_deref().unwrap_or("Standard"))
            && !summary
                .pricing_dimension_mismatches
                .iter()
                .any(|s| s == "reported service tier has no exact admitted tariff")
        {
            summary
                .pricing_dimension_mismatches
                .push("reported service tier has no exact admitted tariff".into());
        }
        summary.reported_service_tier = Some(actual.clone());
    }
    if let Some(actual) = &actual_region {
        // A missing tariff region is an unrestricted price, not an invented literal global fence.
        if bound.quote.tariffs.iter().any(|t| {
            t.region
                .as_ref()
                .is_some_and(|expected| region(expected) != region(actual))
        }) && !summary
            .pricing_dimension_mismatches
            .iter()
            .any(|s| s == "reported processing region has no exact admitted tariff")
        {
            summary
                .pricing_dimension_mismatches
                .push("reported processing region has no exact admitted tariff".into());
        }
        summary.reported_processing_region = Some(actual.clone());
    }
    let mut balance = before.clone();
    let subscription = current.access_mode == "Subscription"
        || crate::services::router::connections::profiles::profile(&current.endpoint_profile)
            .is_some_and(|p| p.subscription);
    let paid = !subscription
        && balance.classification == "ReportedCharge"
        && summary
            .reported_cost
            .as_ref()
            .is_some_and(|cost| cost.currency == "USD");
    if bound.quote.classification != CostClassification::LocalFree
        && !paid
        && !summary.pricing_dimension_mismatches.is_empty()
    {
        balance.priced_micro_usd = None;
        balance.monetary_complete = false;
        balance.classification = "Untracked".into();
        balance.reserved_micro_usd = bound.reservation_micro_usd;
        summary.unknown_components = summary.pricing_dimension_mismatches.clone();
    }
    balance.revision += 1;
    balance.summary = serde_json::to_value(&summary)
        .map_err(|_| StorageError::InvalidRecord("invalid response dimensions".into()))?;
    tx.usage()
        .put_attempt_balance(&balance, Some(before.revision))?;
    let delta = if before.priced_micro_usd == balance.priced_micro_usd {
        None
    } else {
        Some(
            balance
                .priced_micro_usd
                .unwrap_or(0)
                .checked_sub(before.priced_micro_usd.unwrap_or(0))
                .ok_or_else(|| {
                    StorageError::InvalidRecord("dimension correction overflow".into())
                })?,
        )
    };
    accounting::log(
        tx,
        &current,
        "InferenceDimensionsObserved",
        delta,
        json!({"serviceTier":actual_tier,"processingRegion":actual_region,"mismatches":summary.pricing_dimension_mismatches,"quotedTariffUnchanged":true}),
    )?;
    if let Some(feature) = &current.feature_id
        && let Some(work) = tx.work().work(feature)?
    {
        budget::reconcile(tx, control, &work)?;
    }
    Ok(())
}
