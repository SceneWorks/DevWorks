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

#[test]
fn missing_current_indexes_have_explicit_partial_scope_and_no_fake_legacy_completion() {
    let raw = fixture("dw28-current-mixed-index-partial");
    let mut value = result(raw.clone()).unwrap();
    assert_eq!(raw["value"]["completion"], "Partial");
    assert_eq!(
        raw["value"]["scopeCoverage"]["unavailableRepositories"][0]["repositoryId"],
        "repo-b"
    );
    assert!(raw["value"]["result"]["value"].get("total").is_none());
    value.retain_revision(1);
    assert!(value.check_revision(1).is_err());
    assert!(result(fixture("dw28-reject-partial-as-complete")).is_err());
    assert!(result(fixture("dw28-reject-partial-with-complete-coverage")).is_err());
    assert!(
        serde_json::from_value::<devworks_core::protocol::data::QueryCompletion>(json!("Partial"))
            .is_err(),
        "unqualified retrieval completion stays unchanged"
    );
}
#[test]
fn module_analysis_preserves_actual_records_and_separates_indexed_counts() {
    let raw = fixture("dw28-module-not-analyzed-indexed-facts");
    result(raw.clone()).unwrap();
    let module = &raw["value"]["result"]["value"]["internalModules"][0];
    assert_eq!(module["state"], "NotAnalyzed");
    assert!(module.get("recorded").is_none());
    assert_eq!(module["observedNodeCounts"][0]["count"], "7");
    let recorded = fixture("dw28-recorded-module-full-source-fields");
    result(recorded.clone()).unwrap();
    let record = &recorded["value"]["result"]["value"]["internalModules"][0]["recorded"];
    assert_eq!(record["modelUsed"], "recorded-model");
    assert!(record.get("sourceHash").is_none());
    assert!(record.get("programTotals").is_none());
    assert_eq!(record["endpoints"][0]["route"], "/orders");
    result(fixture("dw28-unavailable-module-counts-unknown")).unwrap();
    for id in [
        "dw28-reject-module-invented-recorded-state",
        "dw28-reject-unavailable-module-fake-zero",
        "dw28-reject-module-cross-scope-drilldown",
        "dw28-reject-metadata-only-module-fallback",
    ] {
        assert!(result(fixture(id)).is_err(), "{id}");
    }
}
#[test]
fn repository_node_scope_is_required_only_for_the_qualified_current_revision() {
    assert!(admit(
        "repositories.nodes",
        Mode::Query,
        &fixture("dw28-input-repositories.nodes")
    ));
    for id in [
        "dw28-reject-missing-repository-scope3",
        "dw28-reject-null-repository-scope3",
    ] {
        assert!(
            !admit("repositories.nodes", Mode::Query, &fixture(id)),
            "{id}"
        );
    }
    assert!(admit(
        "repositories.nodes",
        Mode::Query,
        &fixture("dw28-legacy-repository-scope-from-host")
    ));
    let mut legacy = fixture("dw28-legacy-repository-scope-from-host");
    legacy["schemaRevision"] = json!(3);
    assert!(
        !admit("repositories.nodes", Mode::Query, &legacy),
        "old-style views do not bypass mandatory current scope"
    );
}

#[test]
fn trust_current_scope_and_optional_retained_pin_are_strict_revision_fields() {
    assert!(admit(
        "code.set_trust",
        Mode::Command,
        &fixture("dw28-trust-explicit-retained-pin")
    ));
    for id in [
        "dw28-reject-trust-null-pin",
        "dw28-reject-trust-missing-scope3",
        "dw28-reject-trust-null-scope3",
    ] {
        assert!(
            !admit("code.set_trust", Mode::Command, &fixture(id)),
            "{id}"
        );
    }
    let mut legacy = fixture("dw28-input-code.set_trust");
    legacy["schemaRevision"] = json!(1);
    for key in [
        "scope",
        "pinId",
        "unitKey",
        "expectedViewDigest",
        "expectedContentIdentity",
    ] {
        legacy["command"].as_object_mut().unwrap().remove(key);
    }
    assert!(admit("code.set_trust", Mode::Command, &legacy));
    legacy["schemaRevision"] = json!(3);
    assert!(!admit("code.set_trust", Mode::Command, &legacy));
}

#[test]
fn architecture_preserves_unknown_package_total_under_work_limit() {
    let mut value = fixture("dw28-result-code.architecture");
    value["value"]["completion"] = json!("WorkLimit");
    value["value"]["result"]["value"]
        .as_object_mut()
        .unwrap()
        .remove("packageCount");
    let decoded = result(value.clone()).unwrap();
    assert!(
        serde_json::to_value(decoded).unwrap()["value"]["result"]["value"]
            .get("packageCount")
            .is_none()
    );
    value["value"]["result"]["value"]["packageCount"] = Value::Null;
    assert!(result(value).is_err());
}

#[test]
fn overview_readme_keeps_only_actual_same_view_source_provenance() {
    let mut value = fixture("dw28-result-code.overview");
    value["value"]["result"]["value"]["repositories"] = json!([{"repositoryId":"repo-a","name":"Actual registered repository","foundational":false}]);
    let source = fixture("dw28-result-code.source")["value"]["result"]["value"]["source"].clone();
    assert!(source.is_object());
    value["value"]["result"]["value"]["repositories"][0]["readmeSource"] = source;
    result(value.clone()).unwrap();
    let mut wrong = value.clone();
    wrong["value"]["result"]["value"]["repositories"][0]["readmeSource"]["snapshot"]["repositoryId"] =
        json!("wrong-repo");
    assert!(result(wrong).is_err());
    let mut wrong = value.clone();
    wrong["value"]["result"]["value"]["repositories"][0]["readmeSource"]["snapshot"]["head"]["hex"] =
        json!("c".repeat(40));
    assert!(result(wrong).is_err());
    let mut absent = value.clone();
    absent["value"].as_object_mut().unwrap().remove("view");
    let metadata = &mut absent["value"]["result"]["value"];
    metadata["graphCoverage"] = json!({"state":"Unavailable","gaps":[],"moreGaps":false});
    metadata["unavailableReason"] = json!("not_indexed");
    metadata.as_object_mut().unwrap().remove("graphTotals");
    assert!(result(absent).is_err());
    value["value"]["result"]["value"]["repositories"][0]["readmeSource"] = Value::Null;
    assert!(result(value).is_err());
}

#[test]
fn overview_unknown_foundation_is_distinct_from_recorded_false() {
    let value = json!({"repositoryId":"repo-a","name":"Captured registration"});
    let unknown: devworks_core::protocol::records::CodeOverviewRepository =
        serde_json::from_value(value.clone()).unwrap();
    assert!(unknown.foundational.is_none());
    assert!(
        serde_json::to_value(unknown)
            .unwrap()
            .get("foundational")
            .is_none()
    );
    let mut known = value.clone();
    known["foundational"] = json!(false);
    let known: devworks_core::protocol::records::CodeOverviewRepository =
        serde_json::from_value(known).unwrap();
    assert_eq!(known.foundational, Some(false));
    let mut null = value;
    null["foundational"] = Value::Null;
    assert!(
        serde_json::from_value::<devworks_core::protocol::records::CodeOverviewRepository>(null)
            .is_err()
    );
}
