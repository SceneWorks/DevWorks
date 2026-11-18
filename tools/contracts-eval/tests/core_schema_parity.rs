//! Core's protocol types and the contracts-eval probe's DTOs describe the same contracts. The
//! probe emits the committed schemas under `docs/architecture/contracts/`. Core's types are
//! what the product decodes. Every root schema the probe emits for a type core also defines
//! must equal core's schema of that type. Before comparing, presentation-only differences are
//! normalised away (see [`normalise`]); any other difference fails, naming the root schema and
//! the JSON pointer where the two first differ.
//!
//! It runs with this probe's tests (`cargo test --manifest-path tools/contracts-eval/Cargo.toml`),
//! which CI runs in the `generated contracts are current` job.
//!
//! `DEVWORKS_SCHEMA_DUMP=<dir>` also writes both normalised documents of every root, for review.
use schemars::schema_for;
use serde_json::Value;

/// Each root schema the probe emits (by its committed document name) with core's schema and the
/// probe's, both as emitted by `schema_for!` before any revision transform.
fn roots() -> Vec<(&'static str, Value, Value)> {
    use devworks_contract_probe as probe;
    use devworks_core::protocol as core;
    use devworks_extension_protocol as ext;
    macro_rules! pair {
        ($name:literal, $core:ty, $probe:ty) => {
            (
                $name,
                serde_json::to_value(schema_for!($core)).unwrap(),
                serde_json::to_value(schema_for!($probe)).unwrap(),
            )
        };
    }
    vec![
        pair!(
            "domain-query-reply",
            core::results::DomainQueryReply,
            probe::domain_generated::DomainQueryReply
        ),
        pair!(
            "domain-command-reply",
            core::results::DomainCommandReply,
            probe::domain_generated::DomainCommandReply
        ),
        pair!(
            "operation-result",
            core::results::OperationResult,
            probe::domain_generated::OperationResult
        ),
        pair!(
            "operation-completion",
            core::results::OperationCompletion,
            probe::domain_generated::OperationCompletion
        ),
        pair!(
            "command-request",
            core::shared::CommandRequest,
            probe::CommandRequest
        ),
        pair!(
            "command-reply",
            core::shared::CommandReply,
            probe::CommandReply
        ),
        pair!(
            "query-request",
            core::shared::QueryRequest,
            probe::QueryRequest
        ),
        pair!("query-reply", core::shared::QueryReply, probe::QueryReply),
        pair!("event", core::shared::Event, probe::Event),
        pair!(
            "graph-read-request",
            core::data::GraphReadRequest,
            probe::data_delivery::GraphReadRequest
        ),
        pair!(
            "graph-page",
            core::data::GraphPage,
            probe::data_delivery::GraphPage
        ),
        pair!(
            "resolved-view-set",
            core::data::ResolvedViewSet,
            probe::data_delivery::ResolvedViewSet
        ),
        pair!(
            "retrieval-request",
            core::data::RetrievalRequest,
            probe::data_delivery::RetrievalRequest
        ),
        pair!(
            "retrieval-page",
            core::data::RetrievalPage,
            probe::data_delivery::RetrievalPage
        ),
        pair!(
            "index-command-request",
            core::data::IndexCommandRequest,
            probe::data_delivery::IndexCommandRequest
        ),
        pair!(
            "publication-request",
            core::data::PublicationRequest,
            probe::data_delivery::PublicationRequest
        ),
        pair!(
            "artifact-command-request",
            core::data::ArtifactCommandRequest,
            probe::data_delivery::ArtifactCommandRequest
        ),
        pair!(
            "artifact-command-reply",
            core::data::ArtifactCommandReply,
            probe::data_delivery::ArtifactCommandReply
        ),
        pair!(
            "artifact-query-request",
            core::data::ArtifactQueryRequest,
            probe::data_delivery::ArtifactQueryRequest
        ),
        pair!(
            "artifact-deletion-request",
            core::data::ArtifactDeletionRequest,
            probe::data_delivery::ArtifactDeletionRequest
        ),
        pair!(
            "delivery-barrier",
            core::data::DeliveryBarrier,
            probe::data_delivery::DeliveryBarrier
        ),
        pair!(
            "git-command-request",
            core::data::GitCommandRequest,
            probe::data_delivery::GitCommandRequest
        ),
        pair!(
            "delivery-snapshot",
            core::data::DeliverySnapshot,
            probe::data_delivery::DeliverySnapshot
        ),
        pair!(
            "release-command-request",
            core::data::ReleaseCommandRequest,
            probe::data_delivery::ReleaseCommandRequest
        ),
        pair!(
            "inference-request",
            core::provider::InferenceRequest,
            probe::provider::InferenceRequest
        ),
        pair!(
            "route-decision",
            core::provider::RouteDecision,
            probe::provider::RouteDecision
        ),
        pair!(
            "attempt-envelope",
            core::provider::AttemptEnvelope,
            probe::provider::AttemptEnvelope
        ),
        pair!(
            "capability-snapshot",
            core::provider::CapabilitySnapshot,
            probe::provider::CapabilitySnapshot
        ),
        pair!(
            "inference-spec",
            core::provider::InferenceSpec,
            probe::provider::InferenceSpec
        ),
        pair!(
            "provider-event",
            core::provider::ProviderEvent,
            probe::provider::ProviderEvent
        ),
        pair!(
            "provider-codec-event",
            ext::codec::ProviderCodecEvent,
            probe::provider::ProviderCodecEvent
        ),
        pair!(
            "transcript-batch",
            core::provider::TranscriptBatch,
            probe::provider::TranscriptBatch
        ),
        pair!(
            "session-command-request",
            core::provider::SessionCommandRequest,
            probe::provider::SessionCommandRequest
        ),
        pair!(
            "session-command-reply",
            core::provider::SessionCommandReply,
            probe::provider::SessionCommandReply
        ),
        pair!(
            "session-query-request",
            core::provider::SessionQueryRequest,
            probe::provider::SessionQueryRequest
        ),
        pair!(
            "transcript-page",
            core::provider::TranscriptPage,
            probe::provider::TranscriptPage
        ),
        pair!(
            "package-manifest",
            ext::package::PackageManifest,
            probe::package::PackageManifest
        ),
        pair!(
            "package-host-request",
            ext::package::HostRequest,
            probe::package::HostRequest
        ),
        pair!(
            "package-request",
            ext::package::PackageRequest,
            probe::package::PackageRequest
        ),
        pair!(
            "package-notification",
            ext::package::PackageNotification,
            probe::package::PackageNotification
        ),
        pair!(
            "package-response",
            ext::package::RpcResponse,
            probe::package::RpcResponse
        ),
        pair!(
            "addon-command-request",
            core::addons::AddonCommandRequest,
            probe::package::AddonCommandRequest
        ),
        pair!(
            "addon-command-reply",
            core::addons::AddonCommandReply,
            probe::package::AddonCommandReply
        ),
    ]
}

/// Removes what only presents a schema and does not change what it accepts:
/// - `description` annotations (documentation text);
/// - the order of `oneOf` / `anyOf` alternatives;
/// - the names schemars gives a generic family by first use (`Page`, `Page2`, … `Page11`): each
///   member is renamed after its content, so the same page shape has the same name on both
///   sides whatever order the types were first met in.
fn normalise(mut v: Value) -> Value {
    tidy(&mut v);
    for _ in 0..2 {
        rename_families(&mut v);
        tidy(&mut v);
    }
    v
}

/// Drops `description` annotations and sorts union alternatives, everywhere but inside a
/// `properties` map (whose keys are member names, not keywords).
fn tidy(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.remove("description");
            for (k, x) in m.iter_mut() {
                if k == "properties" || k == "$defs" {
                    if let Value::Object(members) = x {
                        members.values_mut().for_each(tidy);
                    }
                } else {
                    tidy(x);
                }
            }
            for key in ["oneOf", "anyOf"] {
                if let Some(Value::Array(alts)) = m.get_mut(key) {
                    alts.sort_by_cached_key(|a| a.to_string());
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(tidy),
        _ => {}
    }
}

/// Renames every member of a numbered definition family after its content.
fn rename_families(v: &mut Value) {
    let Some(Value::Object(defs)) = v.get("$defs") else {
        return;
    };
    let base = |name: &str| {
        name.trim_end_matches(|c: char| c.is_ascii_digit())
            .to_owned()
    };
    let mut families: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for name in defs.keys() {
        families.entry(base(name)).or_default().push(name.clone());
    }
    let renames: Vec<(String, String)> = families
        .into_iter()
        .filter(|(_, members)| members.len() > 1)
        .flat_map(|(family, members)| {
            members.into_iter().map(move |name| {
                let content = defs[&name].to_string();
                let digest = devworks_core::fingerprint::sha256_hex(content.as_bytes());
                (name, format!("{family}~{}", &digest[..12]))
            })
        })
        .collect();
    fn retarget(v: &mut Value, renames: &[(String, String)]) {
        match v {
            Value::Object(m) => {
                if let Some(Value::String(r)) = m.get_mut("$ref")
                    && let Some(name) = r.strip_prefix("#/$defs/")
                    && let Some((_, to)) = renames.iter().find(|(from, _)| from == name)
                {
                    *r = format!("#/$defs/{to}");
                }
                m.values_mut().for_each(|x| retarget(x, renames));
            }
            Value::Array(a) => a.iter_mut().for_each(|x| retarget(x, renames)),
            _ => {}
        }
    }
    retarget(v, &renames);
    if let Some(Value::Object(defs)) = v.get_mut("$defs") {
        for (from, to) in &renames {
            if let Some(def) = defs.remove(from) {
                defs.insert(to.clone(), def);
            }
        }
    }
}

/// The JSON pointer of the first difference between `a` and `b`, in key order.
fn first_difference(a: &Value, b: &Value, at: String) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            keys.into_iter().find_map(|k| match (x.get(k), y.get(k)) {
                (Some(p), Some(q)) => first_difference(p, q, format!("{at}/{k}")),
                _ => Some(format!("{at}/{k}")),
            })
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => x
            .iter()
            .zip(y)
            .enumerate()
            .find_map(|(i, (p, q))| first_difference(p, q, format!("{at}/{i}"))),
        _ if a == b => None,
        _ => Some(if at.is_empty() { "/".into() } else { at }),
    }
}

#[test]
fn every_root_schema_core_defines_equals_the_probes_once_presentation_is_normalised() {
    let dump = std::env::var_os("DEVWORKS_SCHEMA_DUMP").map(std::path::PathBuf::from);
    let mut drift = Vec::new();
    for (name, core, probe) in roots() {
        let (core, probe) = (normalise(core), normalise(probe));
        if let Some(dir) = &dump {
            std::fs::create_dir_all(dir).unwrap();
            for (side, v) in [("core", &core), ("probe", &probe)] {
                std::fs::write(
                    dir.join(format!("{name}.{side}.json")),
                    serde_json::to_string_pretty(v).unwrap(),
                )
                .unwrap();
            }
        }
        if let Some(at) = first_difference(&core, &probe, String::new()) {
            drift.push(format!("{name} at {at}"));
        }
    }
    assert!(
        drift.is_empty(),
        "root schemas differ:\n{}",
        drift.join("\n")
    );
}
