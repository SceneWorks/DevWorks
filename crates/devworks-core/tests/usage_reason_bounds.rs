use devworks_core::protocol::records::UsageCostSnapshot;
use devworks_core::protocol::support::{Check, Text};
use serde_json::json;

#[test]
fn monetary_coverage_retains_all_128_unknown_unit_dimensions() {
    let reason = (0..128)
        .map(|i| {
            let modality = format!("{i}m");
            format!("{modality:m<128} {} {}", "c".repeat(128), "u".repeat(128))
        })
        .collect::<Vec<_>>()
        .join("; ");
    assert!(reason.chars().count() > 4096 && reason.chars().count() <= 65536);
    assert!(serde_json::from_value::<Text>(json!(reason)).is_err());
    let value = json!({"priceKind":"Untracked","priceOrigin":"Untracked", "subscriptionEquivalent":false,
        "incomplete":false,"tariffs":[],"monetaryComplete":false,"untrackedReason":reason});
    let decoded: UsageCostSnapshot = serde_json::from_value(value.clone()).unwrap();
    decoded.check().unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
}
