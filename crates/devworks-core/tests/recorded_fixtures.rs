//! Replays every recorded positive/negative fixture message through the production native path
//! and asserts the recorded typed outcome. Request entries go through `admission::decode` under a
//! host selection derived from the message (as a host that selected exactly what it claims);
//! reply/result/event entries go through strict framing plus the adopted DTO checks. An entry
//! `<schema>.rN` is described by schema revision N's document; a result entry is read by the
//! strict reader of its document's revision.
mod common;

use devworks_core::admission::{self, Selection};
use devworks_core::framing;
use devworks_core::protocol::addons::AddonCommandReply;
use devworks_core::protocol::data::*;
use devworks_core::protocol::provider::*;
use devworks_core::protocol::results::*;
use devworks_core::protocol::support::Revisioned;
use devworks_core::protocol::*;
use devworks_core::registry::{self, Envelope, Mode, SchemaRevision};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Fixture {
    id: String,
    entry: String,
    raw: String,
    typed_valid: bool,
    #[allow(dead_code)]
    schema_valid: bool,
}

/// Entries owned by the public package protocol crate; asserted in its own tests.
const EXTENSION_ENTRIES: [&str; 6] = [
    "package-manifest",
    "package-host-request",
    "package-request",
    "package-notification",
    "package-response",
    "provider-codec-event",
];

fn dto<T: DeserializeOwned>(
    raw: &str,
    check: impl FnOnce(&T) -> Result<(), String>,
) -> Result<(), String> {
    let value = framing::parse(raw.as_bytes()).map_err(|e| e.to_string())?;
    let typed: T = serde_json::from_value(value).map_err(|e| e.to_string())?;
    check(&typed)
}
fn ok<T>(_: &T) -> Result<(), String> {
    Ok(())
}
/// `<schema>.rN` is schema revision N's document of `<schema>`; a plain name is revision 1.
fn entry_revision(entry: &str) -> (&str, u32) {
    entry
        .rsplit_once(".r")
        .and_then(|(base, r)| Some((base, r.parse::<u32>().ok().filter(|r| *r >= 2)?)))
        .unwrap_or((entry, 1))
}
/// The strict reader of `revision`: a member introduced later is a decoding failure.
fn at_revision<T: Revisioned + Clone + serde::Serialize>(
    v: &T,
    revision: u32,
) -> Result<(), String> {
    let mut at = v.clone();
    at.retain_revision(revision);
    if serde_json::to_value(&at).unwrap() != serde_json::to_value(v).unwrap() {
        return Err(format!(
            "a member introduced after schema revision {revision} is present"
        ));
    }
    Ok(())
}
fn s(r: Result<(), &'static str>) -> Result<(), String> {
    r.map_err(str::to_owned)
}

/// Recorded specialized-envelope variants that are valid DTO shapes but are selected by no
/// catalog operation in the family register, so the gateway can never admit them.
const UNBOUND_VARIANT_CASES: [&str; 4] = [
    "artifact-Replace",
    "artifact-DeleteVersion",
    "artifact-DetachProject",
    "artifact-query-ResolveCitation",
];

/// Host selections for a request fixture: every registered operation whose envelope and selector
/// the message claims (several for disclosed aliases). `false` means no operation selects it and
/// the returned selection is a mismatching one.
fn selections(entry: &str, value: &Value) -> (Vec<(&'static str, Mode)>, bool) {
    let entry = entry_revision(entry).0;
    if entry == "domain-request" {
        let mode = match value.pointer("/mode").and_then(Value::as_str) {
            Some("Command") => Mode::Command,
            _ => Mode::Query,
        };
        let op = value
            .pointer("/body/kind")
            .and_then(Value::as_str)
            .unwrap_or("");
        return (
            vec![(
                registry::find(op).map_or("unregistered.operation", |d| d.name),
                mode,
            )],
            true,
        );
    }
    let candidates: Vec<_> = registry::all()
        .iter()
        .filter(|d| matches!(d.envelope, Envelope::Specialized { schema } if schema == entry))
        .collect();
    assert!(
        !candidates.is_empty(),
        "no registered operation uses envelope {entry}"
    );
    let matching: Vec<_> = candidates
        .iter()
        .filter(|d| {
            d.selector.is_none_or(|sel| {
                value.pointer(sel.pointer).and_then(Value::as_str) == Some(sel.equals)
            })
        })
        .map(|d| (d.name, d.modes[0].mode))
        .collect();
    if matching.is_empty() {
        (
            vec![(candidates[0].name, candidates[0].modes[0].mode)],
            false,
        )
    } else {
        (matching, true)
    }
}

/// DTO-level validity of a specialized envelope, independent of operation binding.
fn envelope_dto(case: &Fixture) -> Result<(), String> {
    let raw = case.raw.as_str();
    match entry_revision(&case.entry).0 {
        "artifact-command-request" => dto::<ArtifactCommandRequest>(raw, |v| s(v.validate())),
        "artifact-query-request" => dto::<ArtifactQueryRequest>(raw, |v| s(v.validate())),
        "graph-read-request" => dto::<GraphReadRequest>(raw, |v| s(v.validate())),
        "git-command-request" => dto::<GitCommandRequest>(raw, |v| s(v.validate())),
        "index-command-request" => dto::<IndexCommandRequest>(raw, ok),
        "release-command-request" => dto::<ReleaseCommandRequest>(raw, |v| s(v.validate())),
        "addon-command-request" => dto::<addons::AddonCommandRequest>(raw, |v| s(v.validate())),
        "command-request" => dto::<CommandRequest>(raw, |v| s(v.validate())),
        "query-request" => dto::<QueryRequest>(raw, |v| s(v.validate())),
        other => panic!("{}: no DTO decoder for {other}", case.id),
    }
}

fn request(case: &Fixture) -> Result<(), String> {
    let value = framing::parse(case.raw.as_bytes()).unwrap_or(Value::Null);
    let (selected, bound) = selections(&case.entry, &value);
    if !bound {
        // Never admitted by the gateway, whatever the DTO says.
        let (operation, mode) = selected[0];
        let refusal =
            admission::decode(Selection { operation, mode }, case.raw.as_bytes()).unwrap_err();
        let dto = envelope_dto(case);
        if dto.is_ok() {
            assert!(
                matches!(
                    refusal.error,
                    admission::AdmissionError::SelectorMismatch { .. }
                ),
                "{}: unbound variant refused for the wrong reason: {}",
                case.id,
                refusal.error
            );
            assert!(
                UNBOUND_VARIANT_CASES.contains(&case.id.as_str()),
                "{}: new unbound DTO variant",
                case.id
            );
        }
        return dto;
    }
    let mut outcomes = Vec::new();
    for (operation, mode) in selected {
        let decoded = admission::decode(Selection { operation, mode }, case.raw.as_bytes());
        outcomes.push(
            decoded
                .map(|d| (d.descriptor.name, d.fingerprint))
                .map_err(|r| r.error.to_string()),
        );
    }
    let first_ok = outcomes[0].is_ok();
    assert!(
        outcomes.iter().all(|o| o.is_ok() == first_ok),
        "{}: aliases disagree: {outcomes:?}",
        case.id
    );
    if first_ok && outcomes.len() > 1 {
        // Shared input shapes never merge entitlements or intent identity.
        let mut prints: Vec<_> = outcomes
            .iter()
            .map(|o| o.as_ref().unwrap().1.clone())
            .collect();
        prints.dedup();
        assert_eq!(
            prints.len(),
            outcomes.len(),
            "{}: alias fingerprints collide",
            case.id
        );
    }
    outcomes.remove(0).map(|_| ())
}

fn check(case: &Fixture) -> Result<(), String> {
    let raw = case.raw.as_str();
    let (entry, revision) = entry_revision(&case.entry);
    match entry {
        "domain-request"
        | "command-request"
        | "query-request"
        | "graph-read-request"
        | "artifact-command-request"
        | "artifact-query-request"
        | "artifact-deletion-request"
        | "git-command-request"
        | "index-command-request"
        | "release-command-request"
        | "retrieval-request"
        | "session-command-request"
        | "session-query-request"
        | "addon-command-request" => request(case),
        "domain-query-reply" => dto::<DomainQueryReply>(raw, |v| v.validate()),
        "domain-command-reply" => dto::<DomainCommandReply>(raw, validate_command_reply),
        "operation-result" => dto::<OperationResult>(raw, |v| {
            at_revision(v, revision)?;
            // A union alternative introduced later (the command aggregate of an event) cannot be
            // cleared from a value; its revision refuses it, as in the recorded probe.
            if let OperationResult::HistoryEvents { value } = v {
                for event in &value.items.0 {
                    s(event.validate_revision(revision))?;
                    s(event.validate())?;
                }
            }
            v.check()
        }),
        "operation-completion" => dto::<OperationCompletion>(raw, |v| v.validate()),
        "command-reply" => dto::<CommandReply>(raw, |v| s(v.validate())),
        "query-reply" => dto::<QueryReply>(raw, |v| s(v.validate())),
        "event" => dto::<Event>(raw, |v| s(v.validate())),
        "inference-request" => dto::<InferenceRequest>(raw, |v| s(v.validate())),
        "provider-event" => dto::<ProviderEvent>(raw, |v| s(v.validate())),
        "transcript-batch" => dto::<TranscriptBatch>(raw, |v| s(v.validate())),
        "transcript-page" => dto::<TranscriptPage>(raw, |v| s(v.validate())),
        "inference-spec" => dto::<InferenceSpec>(raw, |v| s(v.validate())),
        "attempt-envelope" => dto::<AttemptEnvelope>(raw, |v| s(v.validate())),
        "route-decision" => dto::<RouteDecision>(raw, ok),
        "capability-snapshot" => dto::<CapabilitySnapshot>(raw, ok),
        "session-command-reply" => dto::<SessionCommandReply>(raw, |v| s(validate_domain_reply(v))),
        "addon-command-reply" => dto::<AddonCommandReply>(raw, |v| s(validate_domain_reply(v))),
        "artifact-command-reply" => {
            dto::<ArtifactCommandReply>(raw, |v| s(validate_domain_reply(v)))
        }
        "graph-page" => dto::<GraphPage>(raw, |v| s(v.validate())),
        "resolved-view-set" => dto::<ResolvedViewSet>(raw, |v| s(v.validate())),
        "retrieval-page" => dto::<RetrievalPage>(raw, |v| s(v.validate())),
        "delivery-barrier" => dto::<DeliveryBarrier>(raw, |v| s(v.validate())),
        "delivery-snapshot" => dto::<DeliverySnapshot>(raw, |v| s(v.validate())),
        "publication-request" => dto::<PublicationRequest>(raw, |v| s(v.validate())),
        other => panic!("{}: unhandled fixture entry {other}", case.id),
    }
}

fn run(file: &str, expected_cases: usize) -> usize {
    let text = std::fs::read_to_string(common::contracts_dir().join(file)).expect("fixture file");
    let cases: Vec<Fixture> = serde_json::from_str(&text).expect("fixture list");
    assert_eq!(
        cases.len(),
        expected_cases,
        "{file} case count changed; update the expectation deliberately"
    );
    let mut failures = Vec::new();
    let mut native = 0;
    for case in &cases {
        if EXTENSION_ENTRIES.contains(&case.entry.as_str()) {
            continue;
        }
        native += 1;
        let outcome = check(case);
        if outcome.is_ok() != case.typed_valid {
            failures.push(format!(
                "{} [{}] expected typedValid={} got {:?}",
                case.id, case.entry, case.typed_valid, outcome
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{file}: {} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
    native
}

#[test]
fn shared_operation_fixtures_match_recorded_outcomes() {
    assert_eq!(run("fixtures.json", 60), 60);
}

#[test]
fn domain_fixtures_match_recorded_outcomes() {
    assert_eq!(run("domain-fixtures.json", 992), 992);
}

#[test]
fn data_delivery_fixtures_match_recorded_outcomes() {
    assert_eq!(run("data-delivery-fixtures.json", 94), 94);
}

#[test]
fn provider_and_package_fixtures_match_recorded_outcomes() {
    // 66 package/codec cases are asserted by devworks-extension-protocol's tests.
    assert_eq!(run("provider-package-fixtures.json", 120), 54);
}

/// Every (operation, mode, schema revision) binding is admitted for at least one recorded message.
#[test]
fn every_catalog_input_has_a_positive_recorded_fixture_admitted_natively() {
    let text =
        std::fs::read_to_string(common::contracts_dir().join("domain-fixtures.json")).unwrap();
    let cases: Vec<Fixture> = serde_json::from_str(&text).unwrap();
    let mut admitted = std::collections::HashSet::new();
    for file in [
        "fixtures.json",
        "domain-fixtures.json",
        "data-delivery-fixtures.json",
        "provider-package-fixtures.json",
    ] {
        let text = std::fs::read_to_string(common::contracts_dir().join(file)).unwrap();
        let cases: Vec<Fixture> = serde_json::from_str(&text).unwrap();
        for case in cases
            .iter()
            .filter(|c| c.typed_valid && !EXTENSION_ENTRIES.contains(&c.entry.as_str()))
        {
            let Ok(value) = framing::parse(case.raw.as_bytes()) else {
                continue;
            };
            let entry = entry_revision(&case.entry).0;
            if !entry.ends_with("-request")
                || entry == "publication-request"
                || entry == "inference-request"
            {
                continue;
            }
            for (operation, mode) in selections(&case.entry, &value).0 {
                if let Ok(d) = admission::decode(Selection { operation, mode }, case.raw.as_bytes())
                {
                    admitted.insert((operation, mode, d.revision));
                }
            }
        }
    }
    drop(cases);
    let missing: Vec<_> = registry::all()
        .iter()
        .flat_map(|d| {
            d.modes
                .iter()
                .flat_map(move |b| d.revisions.iter().map(move |r| (d.name, b.mode, *r)))
        })
        .filter(|binding| !admitted.contains(binding))
        .collect();
    assert!(
        missing.is_empty(),
        "bindings without an admitted recorded fixture: {missing:?}"
    );
    // 359 operation/mode bindings at revision 1, plus revision 2 of artifacts.save and
    // artifacts.list, history.events, .work_lineage, .git_links, .evidence_links, .tool_usage
    // and sessions.briefing, revision 2 of the six resources operations, of sessions.list,
    // .get, .append_message, .start and .end, and of workspace.diagnostics, .logs and
    // .record_diagnostic, revision 2 of router.availability, revision 2 of the fifteen Knowledge
    // operations whose inputs or results carry revision-2 members, and revision 3 of artifacts.save
    // and artifacts.list.
    assert_eq!(admitted.len(), 399);
}

#[test]
fn oversized_frame_is_rejected_before_typed_parsing() {
    let start = r#"{"protocol":"1.0-draft.1","requestKey":"key-start","command":{"kind":"runs.start","rootId":"root-a","expectedWorkRevision":"1","expectedControlRevision":"1"}}"#;
    let selection = Selection {
        operation: "runs.start",
        mode: Mode::Command,
    };
    assert!(admission::decode(selection, start.as_bytes()).is_ok());
    let oversized = " ".repeat(1024 * 1024) + start;
    let refusal = admission::decode(selection, oversized.as_bytes()).unwrap_err();
    assert!(matches!(
        refusal.error,
        admission::AdmissionError::Frame(framing::FrameError::TooLarge { .. })
    ));
}

/// Every `x-since` member a schema declares, with the revision that introduced it.
fn gated_members(v: &Value, out: &mut std::collections::BTreeSet<(String, u64)>) {
    match v {
        Value::Object(m) => {
            if let Some(Value::Object(props)) = m.get("properties") {
                for (name, p) in props {
                    if let Some(since) = p.get("x-since").and_then(Value::as_u64) {
                        out.insert((name.clone(), since));
                    }
                }
            }
            m.values().for_each(|x| gated_members(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| gated_members(x, out)),
        _ => {}
    }
}
fn has_member(v: &Value, member: &str) -> bool {
    match v {
        Value::Object(m) => m.contains_key(member) || m.values().any(|x| has_member(x, member)),
        Value::Array(a) => a.iter().any(|x| has_member(x, member)),
        _ => false,
    }
}

/// Fail-closed revision readers: every `x-since` member of every specialized envelope schema is
/// refused at the revision before the one that introduced it. Each conforming revision-N fixture
/// carrying the member is replayed with its `schemaRevision` lowered (stripped, for revision 1).
#[test]
fn every_gated_envelope_member_is_refused_before_its_revision() {
    let mut cases = Vec::new();
    for file in [
        "fixtures.json",
        "domain-fixtures.json",
        "data-delivery-fixtures.json",
        "provider-package-fixtures.json",
    ] {
        let text = std::fs::read_to_string(common::contracts_dir().join(file)).unwrap();
        cases.extend(serde_json::from_str::<Vec<Fixture>>(&text).unwrap());
    }
    let envelopes: std::collections::BTreeSet<&str> = registry::all()
        .iter()
        .filter_map(|d| match d.envelope {
            Envelope::Specialized { schema } => Some(schema),
            Envelope::Domain => None,
        })
        .collect();
    let mut exercised_members = Vec::new();
    for schema in envelopes {
        let users: Vec<_> = registry::all()
            .iter()
            .filter(|d| d.envelope == (Envelope::Specialized { schema }))
            .collect();
        // The envelope type's full schema: the members of every revision.
        let canonical =
            serde_json::to_value(users[0].modes[0].input_schema(SchemaRevision::MAX)).unwrap();
        let mut gated = std::collections::BTreeSet::new();
        gated_members(&canonical, &mut gated);
        for (member, since) in gated {
            let mut exercised = 0;
            for case in cases.iter().filter(|c| {
                c.typed_valid && {
                    let (entry, revision) = entry_revision(&c.entry);
                    entry == schema && u64::from(revision) >= since
                }
            }) {
                let value: Value = serde_json::from_str(&case.raw).unwrap();
                if !has_member(&value, &member) {
                    continue;
                }
                for d in &users {
                    let selection = Selection {
                        operation: d.name,
                        mode: d.modes[0].mode,
                    };
                    if admission::decode(selection, case.raw.as_bytes()).is_err() {
                        continue; // selected by another operation sharing the envelope
                    }
                    let mut earlier = value.clone();
                    let envelope = earlier.as_object_mut().unwrap();
                    envelope.remove("schemaRevision");
                    if since - 1 > 1 {
                        envelope.insert("schemaRevision".into(), (since - 1).into());
                    }
                    let refusal = admission::decode(selection, earlier.to_string().as_bytes())
                        .expect_err("an earlier revision must refuse the member");
                    assert!(
                        matches!(refusal.error, admission::AdmissionError::Decode(_)),
                        "{}: {member} at revision {} refused for the wrong reason: {}",
                        case.id,
                        since - 1,
                        refusal.error
                    );
                    exercised += 1;
                }
            }
            assert!(
                exercised > 0,
                "{schema}: no conforming fixture exercises the revision-{since} member {member}"
            );
            exercised_members.push(format!("{schema}/{member}"));
        }
    }
    assert_eq!(
        exercised_members,
        [
            "artifact-command-request/tags",
            "artifact-command-request/workspace",
            "artifact-query-request/tags",
            "artifact-query-request/workspace",
            "query-request/projectId"
        ]
    );
}
