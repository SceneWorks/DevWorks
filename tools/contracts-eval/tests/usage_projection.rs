use devworks_contract_probe::domain_generated::{
    PriceCatalogResult, PriceOverrideResult, RouterUsageResult,
};
use devworks_contract_probe::domain_support::{Check, Revisioned};
use serde_json::{Value, json};

#[test]
fn recorded_legacy_usage_round_trips_only_at_its_recorded_revision() {
    let wire =
        json!({"items":[],"totalTrackedUsd":"0","untrackedRequests":"1","incompleteRequests":"1"});
    let result: RouterUsageResult = serde_json::from_value(wire.clone()).unwrap();
    result.check().unwrap();
    result.check_revision(1).unwrap();
    assert!(result.check_revision(3).is_err());
    assert_eq!(serde_json::to_value(result).unwrap(), wire);
}

#[test]
fn current_usage_cannot_be_projected_into_fabricated_legacy_counters() {
    let wire = json!({"kind":"Filters","filters":{
        "context":{"reportKind":"Filters","range":{"startMs":"0","endMs":"1"},"interval":"day","appliedFilters":{},"readRevision":"1"},
        "items":[],"hasMore":false,"coverage":{"state":"Complete","gaps":[],"moreGaps":false}}});
    let mut result: RouterUsageResult = serde_json::from_value(wire.clone()).unwrap();
    result.check().unwrap();
    result.check_revision(3).unwrap();
    result.retain_revision(1);
    assert!(result.check_revision(1).is_err());
    assert_eq!(serde_json::to_value(result).unwrap(), wire);
}

#[test]
fn clearing_an_override_never_fabricates_a_legacy_zero_rate() {
    let wire = json!({"kind":"Cleared","modelId":"model-a","revision":"2"});
    let mut result: PriceOverrideResult = serde_json::from_value(wire.clone()).unwrap();
    result.check().unwrap();
    result.check_revision(3).unwrap();
    result.retain_revision(1);
    assert!(result.check_revision(1).is_err());
    assert_eq!(serde_json::to_value(result).unwrap(), wire);
}

#[test]
fn legacy_generic_price_page_remains_an_exact_legacy_value() {
    let wire: Value = json!({"items":[],"hasMore":false});
    let result: PriceCatalogResult = serde_json::from_value(wire.clone()).unwrap();
    result.check().unwrap();
    result.check_revision(1).unwrap();
    assert!(result.check_revision(3).is_err());
    assert_eq!(serde_json::to_value(result).unwrap(), wire);
}
