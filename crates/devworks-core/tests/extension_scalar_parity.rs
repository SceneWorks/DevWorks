//! `devworks-extension-protocol` mirrors sixteen of core's wire scalars and references instead of
//! depending on core. This test feeds one corpus of valid and invalid JSON values to both copies
//! of each mirrored type and requires the same decision, the same normalized output and the same
//! JSON Schema, so a change to one copy fails here until the other copy matches.
use devworks_core::protocol::{self as core, provider as core_provider};
use devworks_extension_protocol::scalars as ext;
use schemars::{JsonSchema, schema_for};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

/// Decodes `value` as `T`, runs the type's own post-decode validation and returns the
/// re-serialized value, or `None` when either step rejects it.
fn decode<T: DeserializeOwned + Serialize>(
    value: &Value,
    validate: impl Fn(&T) -> Result<(), &'static str>,
) -> Option<Value> {
    let v: T = serde_json::from_value(value.clone()).ok()?;
    validate(&v).ok()?;
    Some(serde_json::to_value(&v).unwrap())
}

fn same<C, E>(name: &str, corpus: &[Value])
where
    C: DeserializeOwned + Serialize + JsonSchema,
    E: DeserializeOwned + Serialize + JsonSchema,
{
    same_validated::<C, E>(name, corpus, |_| Ok(()), |_| Ok(()));
}

fn same_validated<C, E>(
    name: &str,
    corpus: &[Value],
    core_validate: impl Fn(&C) -> Result<(), &'static str>,
    ext_validate: impl Fn(&E) -> Result<(), &'static str>,
) where
    C: DeserializeOwned + Serialize + JsonSchema,
    E: DeserializeOwned + Serialize + JsonSchema,
{
    assert_eq!(
        serde_json::to_value(schema_for!(C)).unwrap(),
        serde_json::to_value(schema_for!(E)).unwrap(),
        "{name}: core and extension-protocol schemas differ"
    );
    let (mut accepted, mut rejected) = (0, 0);
    for value in corpus {
        let c = decode::<C>(value, &core_validate);
        let e = decode::<E>(value, &ext_validate);
        assert_eq!(
            c, e,
            "{name}: core and extension-protocol disagree on {value}"
        );
        if c.is_some() {
            accepted += 1;
        } else {
            rejected += 1;
        }
    }
    // A corpus that only accepts, or only rejects, cannot detect a drift in the other direction.
    assert!(
        accepted > 0 && rejected > 0,
        "{name}: corpus must hold accepted and rejected values ({accepted} accepted, {rejected} rejected)"
    );
}

/// Shared string corpus for every opaque ID, counter, revision, digest and qualified ID.
fn strings() -> Vec<Value> {
    let digest = "a".repeat(64);
    [
        "".to_owned(),
        "a".into(),
        "A-z_0".into(),
        "a".repeat(128),
        "a".repeat(129),
        "a b".into(),
        "a.b".into(),
        "a/b".into(),
        "é".into(),
        "0".into(),
        "1".into(),
        "01".into(),
        "-1".into(),
        "+1".into(),
        "10".into(),
        "9223372036854775807".into(),
        "9223372036854775808".into(),
        "99999999999999999999".into(),
        "1.0".into(),
        digest.clone(),
        digest.to_uppercase(),
        "a".repeat(63),
        "a".repeat(65),
        format!("{}g", "a".repeat(63)),
        "0123456789abcdef".repeat(4),
        "fixture/object".into(),
        "fix.ture-1_x/ob.ject".into(),
        "Fixture/object".into(),
        "fixture/".into(),
        "/object".into(),
        "a/b/c".into(),
        format!("{}/b", "a".repeat(64)),
        format!("{}/b", "a".repeat(65)),
        format!("a/{}", "b".repeat(65)),
    ]
    .into_iter()
    .map(Value::String)
    .chain([json!(1), json!(null), json!(true), json!(["a"]), json!({})])
    .collect()
}

fn artifact() -> Value {
    json!({"artifactId": "artifact-a", "versionId": "version-a", "sha256": "a".repeat(64),
        "byteLength": "8", "mediaType": "text/plain"})
}
fn schema_ref() -> Value {
    json!({"schemaId": "fixture/object", "revision": "1", "sha256": "a".repeat(64)})
}
fn call_key() -> Value {
    json!({"attemptId": "attempt-a", "nativeCallId": "call-1", "ordinal": "0"})
}

/// The valid record, then each member replaced by an invalid value, removed, or joined by an
/// unknown member.
fn record(valid: Value, replacements: &[(&str, Value)]) -> Vec<Value> {
    let mut corpus = vec![valid.clone()];
    for (member, replacement) in replacements {
        let mut v = valid.clone();
        v[*member] = replacement.clone();
        corpus.push(v);
    }
    for member in valid.as_object().unwrap().keys() {
        let mut v = valid.clone();
        v.as_object_mut().unwrap().remove(member);
        corpus.push(v);
    }
    let mut v = valid.clone();
    v["unknown"] = json!("x");
    corpus.push(v);
    corpus.extend([json!(null), json!("x"), json!([])]);
    corpus
}

#[test]
fn opaque_ids_accept_and_reject_the_same_values() {
    let corpus = strings();
    same::<core::Id, ext::Id>("Id", &corpus);
    same::<core::ProjectId, ext::ProjectId>("ProjectId", &corpus);
    same::<core::RootId, ext::RootId>("RootId", &corpus);
    same::<core::AssignmentId, ext::AssignmentId>("AssignmentId", &corpus);
    same::<core::OperationId, ext::OperationId>("OperationId", &corpus);
    same::<core::AttemptId, ext::AttemptId>("AttemptId", &corpus);
    same::<core::EventId, ext::EventId>("EventId", &corpus);
}

#[test]
fn counters_revisions_digests_and_qualified_ids_accept_and_reject_the_same_values() {
    let corpus = strings();
    same::<core::Counter, ext::Counter>("Counter", &corpus);
    same::<core::Revision, ext::Revision>("Revision", &corpus);
    same::<core_provider::Digest, ext::Digest>("Digest", &corpus);
    same::<core_provider::QualifiedId, ext::QualifiedId>("QualifiedId", &corpus);
}

#[test]
fn references_accept_and_reject_the_same_values() {
    same_validated::<core_provider::ArtifactRef, ext::ArtifactRef>(
        "ArtifactRef",
        &record(
            artifact(),
            &[
                ("artifactId", json!("")),
                ("versionId", json!("a b")),
                ("sha256", json!("A".repeat(64))),
                ("byteLength", json!("01")),
                ("mediaType", json!("")),
                ("mediaType", json!("m".repeat(128))),
                ("mediaType", json!("m".repeat(129))),
            ],
        ),
        core_provider::ArtifactRef::validate,
        ext::ArtifactRef::validate,
    );
    same::<core_provider::SchemaRef, ext::SchemaRef>(
        "SchemaRef",
        &record(
            schema_ref(),
            &[
                ("schemaId", json!("fixture")),
                ("revision", json!("0")),
                ("sha256", json!("a")),
            ],
        ),
    );
    same_validated::<core_provider::ToolCallKey, ext::ToolCallKey>(
        "ToolCallKey",
        &record(
            call_key(),
            &[
                ("attemptId", json!("")),
                ("nativeCallId", json!("")),
                ("nativeCallId", json!("c".repeat(1024))),
                ("nativeCallId", json!("c".repeat(1025))),
                ("ordinal", json!("-1")),
            ],
        ),
        core_provider::ToolCallKey::validate,
        ext::ToolCallKey::validate,
    );
    same::<core_provider::NativeBinding, ext::NativeBinding>(
        "NativeBinding",
        &record(
            json!({"connectionId": "connection-a", "accountRevision": "1",
                "endpointProfile": "OpenAiResponses", "modelId": "model-a", "contextEpoch": "0"}),
            &[
                ("connectionId", json!("")),
                ("accountRevision", json!("0")),
                ("endpointProfile", json!("Other")),
                ("modelId", json!("a/b")),
                ("contextEpoch", json!("x")),
            ],
        ),
    );
    same::<core_provider::CompleteToolCall, ext::CompleteToolCall>(
        "CompleteToolCall",
        &record(
            json!({"call": call_key(), "capabilityId": "fixture/tool",
                "inputSchema": schema_ref(), "arguments": artifact(), "nativeItem": artifact()}),
            &[
                ("call", json!({})),
                ("capabilityId", json!("tool")),
                ("inputSchema", artifact()),
                ("arguments", schema_ref()),
                ("nativeItem", json!(null)),
            ],
        ),
    );
}

#[test]
fn enums_accept_and_reject_the_same_values() {
    let mut profiles: Vec<Value> = [
        "OpenAiResponses",
        "CodexSubscriptionResponses",
        "AnthropicMessages",
        "ClaudeSubscriptionMessages",
        "OpenRouterChat",
        "CompatibleChat",
        "Embedding",
    ]
    .map(Value::from)
    .into();
    profiles.extend([json!("openAiResponses"), json!(""), json!(0)]);
    same::<core_provider::EndpointProfile, ext::EndpointProfile>("EndpointProfile", &profiles);
    let mut outcomes: Vec<Value> = [
        "Completed",
        "ToolCallsReady",
        "Incomplete",
        "Cancelled",
        "Failed",
        "OutcomeUnknown",
    ]
    .map(Value::from)
    .into();
    outcomes.extend([json!("completed"), json!("Pending"), json!(null)]);
    same::<core_provider::TurnOutcome, ext::TurnOutcome>("TurnOutcome", &outcomes);
}
