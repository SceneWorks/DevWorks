//! Every list and length bound the package protocol schema declares is enforced by `validate()`
//! on the frames a helper sends (and on the manifest), at the bound and one past it.
use devworks_extension_protocol::package::{PackageManifest, PackageNotification, RpcResponse};
use serde_json::{Value, json};
use std::path::PathBuf;

fn artifact(n: usize) -> Value {
    json!({"artifactId": format!("a-{n}"), "versionId": "v1", "sha256": "e".repeat(64),
        "byteLength": "4", "mediaType": "application/json"})
}

fn response(result: Value) -> Result<(), &'static str> {
    let frame = json!({"jsonrpc": "2.0", "id": "s:h:1", "result": result});
    serde_json::from_value::<RpcResponse>(frame)
        .expect("decodes")
        .validate()
}

/// Whether `result(n)` validates at `bound` and is refused at `bound + 1`.
fn bounded(bound: usize, result: impl Fn(usize) -> Value) -> (bool, bool) {
    (
        response(result(bound)).is_ok(),
        response(result(bound + 1)).is_err(),
    )
}

#[test]
fn helper_results_beyond_their_declared_list_bounds_are_refused() {
    let initialized = |ids: usize, features: usize| {
        json!({"kind": "Initialized", "selected": {"major": 1, "minor": 0},
            "manifestDigest": "d".repeat(64),
            "contributionIds": (0..ids).map(|n| format!("p/c{n}")).collect::<Vec<_>>(),
            "features": (0..features).map(|n| format!("f{n}")).collect::<Vec<_>>()})
    };
    assert_eq!(
        bounded(256, |n| initialized(n, 0)),
        (true, true),
        "contributionIds"
    );
    assert_eq!(bounded(64, |n| initialized(1, n)), (true, true), "features");
    assert_eq!(
        bounded(
            128,
            |n| json!({"kind": "InvocationFinished", "invocationId": "i-1",
            "outcome": {"kind": "Completed", "artifacts": (0..n).map(artifact).collect::<Vec<_>>()}})
        ),
        (true, true),
        "artifacts"
    );
    assert_eq!(
        bounded(
            64,
            |n| json!({"kind": "EventCommitAccepted", "processingReceipt": "r-1",
            "checkpoint": "1", "stateRevision": "1",
            "operationIds": (0..n).map(|k| format!("op-{k}")).collect::<Vec<_>>()})
        ),
        (true, true),
        "operationIds"
    );
}

#[test]
fn verification_evidence_beyond_its_declared_bound_is_refused() {
    let event = |n: usize| {
        let frame = json!({"jsonrpc": "2.0", "method": "package.event", "params": {
            "processSessionId": "s", "invocationId": "i-1", "invocationToken": "t-1",
            "sequence": "1", "event": {"kind": "Verification", "passed": true,
                "evidence": (0..n).map(artifact).collect::<Vec<_>>()}}});
        serde_json::from_value::<PackageNotification>(frame)
            .expect("decodes")
            .validate()
    };
    assert!(event(128).is_ok());
    assert_eq!(event(129), Err("too much verification evidence"));
}

#[test]
fn a_manifest_version_beyond_its_declared_length_is_refused() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/architecture/contracts/provider-package-fixtures.json");
    let cases: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let valid = cases
        .iter()
        .find(|c| c["entry"] == "package-manifest" && c["typedValid"] == true)
        .expect("a recorded valid manifest");
    let mut manifest: Value = serde_json::from_str(valid["raw"].as_str().unwrap()).unwrap();
    let version = |len: usize| format!("1.0.0-{}", "a".repeat(len - 6));
    manifest["version"] = json!(version(128));
    let at: PackageManifest = serde_json::from_value(manifest.clone()).unwrap();
    assert!(at.validate().is_ok(), "{:?}", at.validate());
    manifest["version"] = json!(version(129));
    let past: PackageManifest = serde_json::from_value(manifest).unwrap();
    assert_eq!(past.validate(), Err("invalid semantic version"));
}
