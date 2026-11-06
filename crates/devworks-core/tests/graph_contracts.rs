//! Qualified Graph wire contracts, not GraphService/storage/runtime acceptance.
#![allow(clippy::result_large_err)]
use devworks_core::admission::{self, Selection};
use devworks_core::protocol::{
    Check,
    data::GraphPage,
    results::OperationResult,
    support::{GraphText, GraphUnitKey, Revisioned},
};
use devworks_core::registry::{self, Mode, SchemaRevision};
use serde_json::{Value, json};

fn fixture(id: &str) -> Value {
    let all: Value = serde_json::from_str(include_str!(
        "../../../docs/architecture/contracts/domain-fixtures.json"
    ))
    .unwrap();
    serde_json::from_str(
        all.as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == id)
            .unwrap()["raw"]
            .as_str()
            .unwrap(),
    )
    .unwrap()
}
fn admit(operation: &str, mode: Mode, value: &Value) -> bool {
    admission::decode(Selection { operation, mode }, value.to_string().as_bytes()).is_ok()
}
fn result(value: Value) -> Result<OperationResult, String> {
    let v: OperationResult = serde_json::from_value(value).map_err(|e| e.to_string())?;
    v.check()?;
    v.check_revision(3)?;
    Ok(v)
}

#[test]
fn all_twenty_graph_operations_admit_their_current_selected_contract() {
    let mut count = 0;
    for d in registry::all()
        .iter()
        .filter(|d| d.name.starts_with("code.") || d.name == "repositories.nodes")
    {
        assert_eq!(d.revisions, &[SchemaRevision(1), SchemaRevision(3)]);
        let mode = if d.name == "code.set_trust" {
            Mode::Command
        } else {
            Mode::Query
        };
        assert!(
            admit(d.name, mode, &fixture(&format!("dw28-input-{}", d.name))),
            "{}",
            d.name
        );
        result(fixture(&format!("dw28-result-{}", d.name))).unwrap();
        count += 1;
    }
    assert_eq!(count, 20);
}
#[test]
fn current_and_legacy_selectors_are_strict_and_revision_bound() {
    let good = fixture("dw28-input-code.call_path");
    let mut old = good.clone();
    old["schemaRevision"] = json!(1);
    assert!(!admit("code.call_path", Mode::Query, &old));
    for field in ["current", "view"] {
        let mut bad = good.clone();
        bad[field] = Value::Null;
        assert!(!admit("code.call_path", Mode::Query, &bad));
    }
    let mut bad = good.clone();
    bad["view"] = json!({"kind":"Exact","viewIds":["view-a"]});
    assert!(!admit("code.call_path", Mode::Query, &bad));
    let mut bad = good.clone();
    bad["query"]["selection"] = Value::Null;
    assert!(!admit("code.call_path", Mode::Query, &bad));
    let mut bad = good;
    bad["query"]["selection"]["target"]["repositoryId"] = Value::Null;
    assert!(!admit("code.call_path", Mode::Query, &bad));
    let mut repo = fixture("dw28-input-repositories.nodes");
    repo["body"]["views"] = Value::Null;
    assert!(!admit("repositories.nodes", Mode::Query, &repo));
}
#[test]
fn trust_accepts_actual_directory_identity_and_refuses_ambiguous_witnesses() {
    let mut trust = fixture("dw28-input-code.set_trust");
    trust["command"]["unitKey"] = json!("/src");
    trust["command"]
        .as_object_mut()
        .unwrap()
        .remove("expectedContentDigest");
    trust["command"]["expectedContentIdentity"] = json!("git-blob-opaque-40-character-identity");
    assert!(admit("code.set_trust", Mode::Command, &trust));
    let mut both = trust.clone();
    both["command"]["expectedContentDigest"] = json!("a".repeat(64));
    assert!(!admit("code.set_trust", Mode::Command, &both));
    for key in ["unitKey", "expectedViewDigest", "expectedContentIdentity"] {
        let mut bad = trust.clone();
        bad["command"][key] = Value::Null;
        assert!(!admit("code.set_trust", Mode::Command, &bad));
    }
    for unit in ["/", "/src", "src/lib.rs"] {
        let value: GraphUnitKey = serde_json::from_value(json!(unit)).unwrap();
        value.check().unwrap();
    }
    for unit in ["", "//src", "../src", "/src/../x", "C:/src", "src\\lib.rs"] {
        assert!(
            serde_json::from_value::<GraphUnitKey>(json!(unit)).is_err(),
            "{unit}"
        );
    }
}
#[test]
fn unavailable_current_view_never_needs_a_fabricated_pin() {
    let value = fixture("dw28-unavailable-own-view-no-fake-pin");
    let v = result(value.clone()).unwrap();
    assert!(value["value"].get("view").is_none());
    assert!(value["value"]["result"]["value"]["stamp"].is_object());
    let mut projected = v;
    projected.retain_revision(1);
    assert!(projected.check_revision(1).is_err());
    assert!(result(fixture("dw28-reject-missing-view-reason")).is_err());
}
#[test]
fn long_source_and_property_fragments_preserve_exact_positions() {
    result(fixture("dw28-long-single-line-source-fragment")).unwrap();
    result(fixture("dw28-large-property-json-fragment")).unwrap();
    result(fixture("dw28-verbatim-nul-source")).unwrap();
    result(fixture("dw28-empty-file")).unwrap();
    assert!(result(fixture("dw28-reject-source-byte-loss")).is_err());
    assert!(result(fixture("dw28-reject-property-gap")).is_err());
    let fragment: GraphText = serde_json::from_value(json!("a\0b")).unwrap();
    assert_eq!(fragment.0.as_bytes(), b"a\0b");
}
#[test]
fn composite_edge_endpoints_and_source_snapshots_cannot_escape_the_view() {
    result(fixture("dw28-result-code.traverse")).unwrap();
    assert!(result(fixture("dw28-reject-edge-endpoint-unit")).is_err());
    assert!(result(fixture("dw28-reject-foreign-view-node")).is_err());
    assert!(result(fixture("dw28-reject-foreign-source-snapshot")).is_err());
    result(fixture("dw28-source-less-external-node")).unwrap();
}
#[test]
fn projection_uses_actual_legacy_citations_and_never_synthesizes_ids() {
    let mut page: GraphPage =
        serde_json::from_value(fixture("dw28-real-legacy-citation-current")["value"].clone())
            .unwrap();
    page.retain_revision(1);
    page.check_revision(1).unwrap();
    page.validate().unwrap();
    assert_eq!(
        serde_json::to_value(page).unwrap()["result"]["nodes"][0]["unitKey"],
        "unit-a"
    );
    for id in [
        "dw28-result-code.architecture",
        "dw28-source-less-external-node",
        "dw28-result-repositories.nodes",
    ] {
        let mut value = result(fixture(id)).unwrap();
        value.retain_revision(1);
        assert!(value.check_revision(1).is_err(), "{id}");
    }
}

#[test]
fn declared_schema_and_metadata_overview_bootstrap_without_fabricated_views() {
    for id in [
        "dw28-static-schema-empty-application",
        "dw28-overview-metadata-with-unknown-graph-totals",
    ] {
        let raw = fixture(id);
        let mut value = result(raw.clone()).unwrap();
        assert!(raw["value"].get("view").is_none());
        assert!(raw["value"]["result"]["value"]["stamp"].is_object());
        value.retain_revision(1);
        assert!(value.check_revision(1).is_err());
    }
    for id in [
        "dw28-reject-viewless-observed-schema",
        "dw28-reject-viewless-schema-without-reason",
        "dw28-reject-metadata-overview-complete-graph",
        "dw28-reject-metadata-overview-fake-zero-totals",
        "dw28-reject-metadata-overview-fallback-dependencies",
        "dw28-reject-empty-fabricated-view",
    ] {
        assert!(result(fixture(id)).is_err(), "{id}");
    }
}
