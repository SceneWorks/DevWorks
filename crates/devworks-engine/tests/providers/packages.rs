//! Native signed-package registry resolution, including legacy exact-bundle reinspection.
use super::*;
use devworks_core::protocol::provider::{SchemaRef, ToolSchema};
use devworks_core::providers::materialize::SchemaDocuments;
use devworks_engine::providers::schemas::RegisteredSchemas;
fn schema_bundle() -> (Vec<u8>, String, ToolSchema, Value) {
    let document = json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false});
    let schema_bytes = devworks_core::jcs::canonicalize(&document)
        .unwrap()
        .into_bytes();
    let reference = schema("schematool/input", &sha256_hex(&schema_bytes));
    let entry = if cfg!(windows) {
        "bin/helper.exe"
    } else {
        "bin/helper"
    };
    let files = [
        (entry.to_string(), helper_bytes()),
        (
            "fixture.json".into(),
            json!({"mode":"normal","inventory":["schematool/echo"]})
                .to_string()
                .into_bytes(),
        ),
        (
            "help.md".into(),
            b"# echo\nReturns the provided text.\n".to_vec(),
        ),
        ("schemas/input.json".into(), schema_bytes),
    ];
    let manifest=json!({"packageId":"schematool","version":"1.0.0","publisherId":"acme","protocolMin":{"major":1,"minor":0},"protocolMax":{"major":1,"minor":0},"minimumHostVersion":"0.1.0","dependencies":[],"runtimes":[],
        "schemaAssets":[{"reference":reference,"path":"schemas/input.json"}],"requestedLimits":{"maxConcurrent":4,"queueCapacity":16,"maxMemoryBytes":"1073741824","streamCreditBytes":65536},
        "targets":[{"targetTriple":host_target_triple(),"entrypoint":entry}],"files":files.iter().map(|(p,b)|json!({"path":p,"sha256":sha256_hex(b),"byteLength":b.len().to_string()})).collect::<Vec<_>>(),"stateSchema":reference,"configurationSchema":reference,"helpFile":"help.md","requestedPermissions":[{"kind":"StateWrite"}],
        "contributions":[{"kind":"Tool","meta":{"id":"schematool/echo","inputSchema":reference,"outputSchema":reference,"helpTopic":"echo","required":false,"cancellation":"Cooperative"},"requestedEffect":"ReadCandidate"}]}).to_string().into_bytes();
    let refs = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect::<Vec<_>>();
    let tool = ToolSchema {
        capability_id: "schematool/echo".to_string().try_into().unwrap(),
        wire_name: "package_echo".into(),
        input_schema: serde_json::from_value::<SchemaRef>(reference).unwrap(),
    };
    (
        write_bundle(&manifest, &refs, Some(&key(1))),
        sha256_hex(&manifest),
        tool,
        document,
    )
}
#[test]
fn registered_provider_tools_use_verified_active_package_schema_metadata_and_legacy_source() {
    let _serial = serial();
    let app = App::new();
    let (bundle, digest, tool, document) = schema_bundle();
    let source = app.save(&app.user, &bundle);
    applied(app.lifecycle(&app.user,"addons.install",json!({"kind":"Install","bundle":source,"expectedRegistryRevision":app.registry().to_string()})));
    let service = app.app().packages().unwrap().service().clone();
    let registry = RegisteredSchemas::new(
        app.store.clone(),
        UseCases::new().with_packages(service.clone()),
        Some(service.clone()),
        app.app().artifacts().clone(),
    );
    let resolved = registry.tool(&app.agent, &tool).unwrap();
    assert_eq!(resolved.schema, document);
    assert!(
        resolved
            .description
            .unwrap()
            .contains("Returns the provided text")
    );
    let mut wrong = tool.clone();
    wrong.capability_id = "schematool/not-registered".to_string().try_into().unwrap();
    assert!(registry.tool(&app.agent, &wrong).is_err());
    // A pre-DW19 compact inspection is recovered only from this exact retained signed source.
    let raw = rusqlite::Connection::open(app.h.root.join("workspace.sqlite")).unwrap();
    let value:String=raw.query_row("SELECT inspection_json FROM package_versions WHERE package_id='schematool' AND manifest_digest=?1",[&digest],|r|r.get(0)).unwrap();
    let mut legacy: Value = serde_json::from_str(&value).unwrap();
    legacy.as_object_mut().unwrap().remove("schemaAssets");
    for c in legacy["contributions"].as_array_mut().unwrap() {
        c.as_object_mut().unwrap().remove("descriptor");
    }
    raw.execute("UPDATE package_versions SET inspection_json=?1 WHERE package_id='schematool' AND manifest_digest=?2",rusqlite::params![legacy.to_string(),digest]).unwrap();
    assert_eq!(registry.tool(&app.agent, &tool).unwrap().schema, document);
    // Even a correctly declared digest cannot qualify bytes changed in the installed version.
    let path = service
        .files()
        .path("schematool", &digest, "schemas/input.json");
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"{}").unwrap();
    assert!(registry.tool(&app.agent, &tool).is_err());
    std::fs::write(&path, original).unwrap();
    applied(app.simple(&app.user, "addons.disable", "Disable", "schematool"));
    assert!(registry.tool(&app.agent, &tool).is_err());
}
