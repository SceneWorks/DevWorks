use devworks_contract_probe::domain_generated::ApprovalViewRecord;
use devworks_contract_probe::domain_support::{Check, DynamicJson, Revisioned};
use serde_json::{Value, json};

fn view(source: Value) -> Value {
    json!({"id":"approval-a","revision":"1","contentDigest":"a".repeat(64),
        "state":"Pending","operationKind":"features.edit_plan","targetId":"feature-a",
        "targetRevision":"1","summary":"Exact immutable reviewed source","reviewedSource":source})
}
fn exact_large_review(source: Value) {
    assert!(serde_json::to_vec(&source).unwrap().len() > 512 * 1024);
    assert!(serde_json::from_value::<DynamicJson>(source.clone()).is_err());
    let wire = view(source);
    let mut typed: ApprovalViewRecord = serde_json::from_value(wire.clone()).unwrap();
    typed.check().unwrap();
    assert_eq!(serde_json::to_value(&typed).unwrap(), wire);
    typed.retain_revision(1);
    assert!(typed.reviewed_source.is_none());
}

#[test]
fn legal_plan_larger_than_metadata_limit_round_trips_without_truncation() {
    let requirements: Vec<Value> = (0..9)
        .map(|i| {
            json!({"id":format!("E{i}"),
        "statement":"x".repeat(65_536),"status":"Confirmed","coveredByTaskIds":[]})
        })
        .collect();
    exact_large_review(json!({"kind":"FeaturePlan","plan":{
        "content":{"artifactId":"artifact-a","versionId":"version-a","sha256":"a".repeat(64),"byteLength":"8","mediaType":"text/plain"},
        "acceptance":[],"repositoryIds":[],"goals":[],"requirements":requirements,"acceptanceTests":[]}}));
}

#[test]
fn legal_settings_larger_than_metadata_limit_preserve_each_bounded_value() {
    let entries: Vec<Value> = (0..2)
        .map(|i| {
            json!({"key":format!("setting-{i}"),"value":{
        "schema":{"schemaId":"fixture/object","revision":"1","sha256":"a".repeat(64)},
        "value":{"text":"x".repeat(300_000)}}})
        })
        .collect();
    exact_large_review(json!({"kind":"Settings","entries":entries}));
}
