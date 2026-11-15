//! A package installed with an older compact inspection (no contribution descriptors retained)
//! gets its tools' command grants from the bundle its record retains, only while that bundle
//! still hashes to the recorded bundle digest.
use super::broker::{application, input_schema, run_calls};
use super::*;
use devworks_core::registry::Mode;
use devworks_engine::package_host::InstanceStatus;

/// Rewrites `package`'s stored version the way an older install stored it: no contribution
/// descriptors and no schema assets; `source` (when given) replaces the retained bundle.
fn store_compact(a: &App, package: &str, source: Option<&Value>) {
    let raw = rusqlite::Connection::open(a.h.root.join("workspace.sqlite")).unwrap();
    let (inspection, provenance): (String, String) = raw
        .query_row(
            "SELECT inspection_json, provenance_json FROM package_versions WHERE package_id=?1",
            [package],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let mut compact: Value = serde_json::from_str(&inspection).unwrap();
    compact.as_object_mut().unwrap().remove("schemaAssets");
    for c in compact["contributions"].as_array_mut().unwrap() {
        c.as_object_mut().unwrap().remove("descriptor");
    }
    let mut provenance: Value = serde_json::from_str(&provenance).unwrap();
    if let Some(source) = source {
        provenance["source"] = source.clone();
    }
    let changed = raw
        .execute(
            "UPDATE package_versions SET inspection_json=?2, provenance_json=?3 WHERE package_id=?1",
            rusqlite::params![package, compact.to_string(), provenance.to_string()],
        )
        .unwrap();
    assert_eq!(changed, 1);
}

/// Disables and re-enables `name`, so its next instance reads the stored version again.
fn restart(a: &App, name: &str) {
    package(a.simple(&a.user, "addons.disable", "Disable", name));
    let until = Instant::now() + PATIENCE;
    while a
        .app()
        .packages()
        .unwrap()
        .instance_status(name)
        .iter()
        .any(|(_, s)| matches!(s, InstanceStatus::Ready | InstanceStatus::Starting))
    {
        assert!(Instant::now() < until, "{name} never stopped");
        std::thread::sleep(Duration::from_millis(20));
    }
    package(a.simple(&a.user, "addons.enable", "Enable", name));
    a.ready(name);
}

/// Runs `name`'s `call` tool as the agent twice: a catalog read, then a disable of `target`.
fn read_then_disable(a: &App, name: &str, target: &str) -> Vec<String> {
    let page = a.save(&a.user, br#"{"page":{"limit":100}}"#);
    let read = json!({"calls": [{"actionKey": "read", "call": application("addons.catalog",
        input_schema("addons.catalog", Mode::Query, Some(2)), json!("$input"))}]});
    let mut progress = run_calls(a, name, read, &page, Some(a.agent.clone()), vec![]).0;
    let command = a.save(
        &a.user,
        json!({"command": {"kind": "Disable", "packageId": target,
            "expectedRegistryRevision": a.registry().to_string()}})
        .to_string()
        .as_bytes(),
    );
    let disable = json!({"calls": [{"actionKey": "disable", "call": application("addons.disable",
        input_schema("addons.disable", Mode::Command, Some(2)), json!("$input"))}]});
    progress.extend(run_calls(a, name, disable, &command, Some(a.agent.clone()), vec![]).0);
    progress
}

#[test]
fn a_compact_install_recovers_its_mutation_grant_only_from_its_verified_bundle() {
    let _s = serial();
    let a = App::new();
    let mut spec = Spec::new("legacy-pack", "1.0.0");
    spec.contributions = vec!["call"];
    spec.effect = "Mutation";
    package(a.install(&a.user, &spec));
    package(a.install(&a.user, &Spec::new("target-pack", "1.0.0")));
    a.ready("legacy-pack");

    // The descriptor is recovered from the exact bundle the record retains.
    store_compact(&a, "legacy-pack", None);
    restart(&a, "legacy-pack");
    assert_eq!(
        read_then_disable(&a, "legacy-pack", "target-pack"),
        vec!["call:read:BrokerCompleted", "call:disable:BrokerCompleted"]
    );
    assert!(!a.record("target-pack").enabled);
    package(a.simple(&a.user, "addons.enable", "Enable", "target-pack"));

    // A retained bundle whose digest no longer matches the recorded one (here another valid
    // bundle of the same package) is refused: reads still work, commands stay denied.
    let mut other = Spec::new("legacy-pack", "1.0.1");
    other.contributions = vec!["call"];
    other.effect = "Mutation";
    let substitute = a.save(&a.user, &bundle(&other).0);
    store_compact(&a, "legacy-pack", Some(&substitute));
    restart(&a, "legacy-pack");
    assert_eq!(
        read_then_disable(&a, "legacy-pack", "target-pack"),
        vec!["call:read:BrokerCompleted", "call:disable:PermissionDenied"]
    );
    assert!(a.record("target-pack").enabled);
    let refusals = refusal_messages(&a, "legacy-pack");
    assert!(
        refusals.len() == 1 && refusals[0].contains("Mismatch"),
        "{refusals:?}"
    );
}

/// The messages of `name`'s recorded descriptor recovery refusals.
fn refusal_messages(a: &App, name: &str) -> Vec<String> {
    let frame = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": "q-logs",
        "body": {"kind": "addons.logs", "packageId": name, "afterSequence": "0", "page": {"limit": 200}}});
    match a
        .app()
        .query(&a.user, "addons.logs", frame.to_string().as_bytes())
        .unwrap()
    {
        DomainQueryReply::Result {
            result: OperationResult::AddonsLogs { value },
            ..
        } => value
            .items
            .0
            .iter()
            .filter(|d| d.code.0 == "PackageDescriptorRecoveryRefused")
            .map(|d| serde_json::to_value(&d.message).unwrap().to_string())
            .collect(),
        other => panic!("{other:?}"),
    }
}
