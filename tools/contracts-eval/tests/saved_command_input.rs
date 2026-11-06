use devworks_contract_probe::domain_support::{Check, DynamicJson, SavedCommandInput};
use serde_json::{Value, json};

fn args(inner: Value) -> Value {
    json!({"installationId":"installation-a","expectedRevision":"1","config":{
        "schema":{"schemaId":"fixture/object","revision":"1","sha256":"a".repeat(64)},"value":inner}})
}
fn legal_inner_survives_argument_wrapping(inner: Value) {
    serde_json::from_value::<DynamicJson>(inner.clone()).unwrap();
    let input = args(inner);
    assert!(serde_json::from_value::<DynamicJson>(input.clone()).is_err());
    let typed: SavedCommandInput = serde_json::from_value(input.clone()).unwrap();
    typed.check().unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), input);
}

#[test]
fn a_legal_near_limit_config_is_not_subject_to_a_second_512k_cap() {
    let inner = json!({"v":"x".repeat(524_273)});
    assert_eq!(serde_json::to_vec(&inner).unwrap().len(), 524_281);
    assert!(serde_json::to_vec(&args(inner.clone())).unwrap().len() > 524_288);
    legal_inner_survives_argument_wrapping(inner);
}

#[test]
fn inner_depth_and_node_limits_are_not_reapplied_to_aggregate_arguments() {
    let mut nested = json!(0);
    for _ in 0..64 {
        nested = json!([nested]);
    }
    legal_inner_survives_argument_wrapping(nested);
    legal_inner_survives_argument_wrapping(Value::Array(vec![json!(0); 9_999]));
    assert!(serde_json::from_value::<DynamicJson>(Value::Array(vec![json!(0); 10_000])).is_err());
}

#[test]
fn the_saved_input_still_enforces_object_frame_size_and_container_depth() {
    assert!(serde_json::from_value::<SavedCommandInput>(json!([])).is_err());
    assert!(serde_json::from_value::<SavedCommandInput>(json!({"kind":"forged"})).is_err());
    assert!(
        SavedCommandInput(json!({"value":"x".repeat(1024 * 1024)}))
            .check()
            .is_err()
    );
    let mut nested = json!(0);
    for _ in 0..100 {
        nested = json!([nested]);
    }
    assert!(SavedCommandInput(json!({"value":nested})).check().is_err());
}
