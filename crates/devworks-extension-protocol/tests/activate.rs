//! `host.activate` carries the committed configuration (value and schema identity) and stays
//! the recorded frame when no configuration was saved.
use devworks_extension_protocol::package::HostRequest;
use serde_json::json;

#[test]
fn activation_carries_the_committed_configuration_and_omits_it_when_none() {
    let with = json!({"jsonrpc": "2.0", "id": "s:h:2", "method": "host.activate", "params": {
        "processSessionId": "s", "registryRevision": "3", "stateGeneration": "2",
        "configurationRevision": "4",
        "configuration": {"schema": {"schemaId": "acme/config", "revision": "1", "sha256": "a".repeat(64)},
            "value": {"threads": 4}}}});
    let req: HostRequest = serde_json::from_value(with.clone()).unwrap();
    req.validate().unwrap();
    let HostRequest::Activate { params, .. } = &req else {
        panic!("{req:?}")
    };
    let c = params.configuration.as_ref().unwrap();
    assert_eq!(c.value, json!({"threads": 4}));
    assert_eq!(c.schema.schema_id.as_str(), "acme/config");
    assert_eq!(serde_json::to_value(&req).unwrap(), with);

    // Without a configuration the frame is the earlier one (the member is omitted).
    let mut without = with.clone();
    without["params"]
        .as_object_mut()
        .unwrap()
        .remove("configuration");
    let req: HostRequest = serde_json::from_value(without.clone()).unwrap();
    assert_eq!(serde_json::to_value(&req).unwrap(), without);
    // Unknown members stay refused.
    let mut extra = with;
    extra["params"]["configuration"]["extra"] = json!(1);
    assert!(serde_json::from_value::<HostRequest>(extra).is_err());
}
