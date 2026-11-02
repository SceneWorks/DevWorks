//! Emit candidate schemas or check structural DTO fixtures. No authority/engine execution.
use devworks_contract_probe::data_delivery::*;
use devworks_contract_probe::domain_generated::*;
use devworks_contract_probe::*;
use devworks_contract_probe::{package::*, provider::*};
use schemars::schema_for;
use serde::Deserialize;
use serde_json::json;
use std::{fs, path::Path};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Fixture {
    id: String,
    entry: String,
    raw: String,
    typed_valid: bool,
    schema_valid: bool,
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("scenario-check") => {
            scenario_check(&args[2])?;
        }
        Some("package-helper") => {
            package_probe::helper()?;
        }
        Some("emit") => {
            let dir = Path::new(&args[2]);
            fs::create_dir_all(dir)?;
            for (name, schema) in [
                ("domain-request", schema_for!(DomainRequest)),
                ("domain-query-reply", schema_for!(DomainQueryReply)),
                ("domain-command-reply", schema_for!(DomainCommandReply)),
                ("operation-result", schema_for!(OperationResult)),
                ("operation-completion", schema_for!(OperationCompletion)),
                ("command-request", schema_for!(CommandRequest)),
                ("command-reply", schema_for!(CommandReply)),
                ("query-request", schema_for!(QueryRequest)),
                ("query-reply", schema_for!(QueryReply)),
                ("event", schema_for!(Event)),
                ("graph-read-request", schema_for!(GraphReadRequest)),
                ("graph-page", schema_for!(GraphPage)),
                ("resolved-view-set", schema_for!(ResolvedViewSet)),
                ("retrieval-request", schema_for!(RetrievalRequest)),
                ("retrieval-page", schema_for!(RetrievalPage)),
                ("index-command-request", schema_for!(IndexCommandRequest)),
                ("publication-request", schema_for!(PublicationRequest)),
                (
                    "artifact-command-request",
                    schema_for!(ArtifactCommandRequest),
                ),
                ("artifact-command-reply", schema_for!(ArtifactCommandReply)),
                ("artifact-query-request", schema_for!(ArtifactQueryRequest)),
                (
                    "artifact-deletion-request",
                    schema_for!(ArtifactDeletionRequest),
                ),
                ("delivery-barrier", schema_for!(DeliveryBarrier)),
                ("git-command-request", schema_for!(GitCommandRequest)),
                ("delivery-snapshot", schema_for!(DeliverySnapshot)),
                (
                    "release-command-request",
                    schema_for!(ReleaseCommandRequest),
                ),
                ("inference-request", schema_for!(InferenceRequest)),
                ("route-decision", schema_for!(RouteDecision)),
                ("attempt-envelope", schema_for!(AttemptEnvelope)),
                ("capability-snapshot", schema_for!(CapabilitySnapshot)),
                ("inference-spec", schema_for!(InferenceSpec)),
                ("provider-event", schema_for!(ProviderEvent)),
                ("provider-codec-event", schema_for!(ProviderCodecEvent)),
                ("transcript-batch", schema_for!(TranscriptBatch)),
                (
                    "session-command-request",
                    schema_for!(SessionCommandRequest),
                ),
                ("session-command-reply", schema_for!(SessionCommandReply)),
                ("session-query-request", schema_for!(SessionQueryRequest)),
                ("transcript-page", schema_for!(TranscriptPage)),
                ("package-manifest", schema_for!(PackageManifest)),
                ("package-host-request", schema_for!(HostRequest)),
                ("package-request", schema_for!(PackageRequest)),
                ("package-notification", schema_for!(PackageNotification)),
                ("package-response", schema_for!(RpcResponse)),
                ("addon-command-request", schema_for!(AddonCommandRequest)),
                ("addon-command-reply", schema_for!(AddonCommandReply)),
            ] {
                let mut value = serde_json::to_value(schema)?;
                value["$id"] = json!(format!("urn:devworks:architecture:1.0-draft.1:{name}"));
                value["$comment"] = json!(
                    "Architecture candidate only. Schema validation is followed by typed range, cross-field and authority validation. Not a public v1 SDK."
                );
                fs::write(
                    dir.join(format!("{name}.schema.json")),
                    serde_json::to_string_pretty(&value)? + "\n",
                )?;
            }
        }
        Some("check") => {
            let cases: Vec<Fixture> = serde_json::from_str(&fs::read_to_string(&args[2])?)?;
            let mut results = Vec::new();
            for case in cases {
                let result = if case.raw.len() > 1024 * 1024 {
                    Err("frame too large".into())
                } else {
                    match case.entry.as_str() {
                        "domain-request" => {
                            decode_domain::<DomainRequest>(&case.raw, |v| v.validate())
                        }
                        "domain-query-reply" => {
                            decode_domain::<DomainQueryReply>(&case.raw, |v| v.validate())
                        }
                        "domain-command-reply" => {
                            decode_domain::<DomainCommandReply>(&case.raw, validate_command_reply)
                        }
                        "operation-result" => decode_domain::<OperationResult>(&case.raw, |v| {
                            devworks_contract_probe::domain_support::Check::check(v)
                        }),
                        "operation-completion" => {
                            decode_domain::<OperationCompletion>(&case.raw, |v| v.validate())
                        }
                        "command-request" => serde_json::from_str::<CommandRequest>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|mut v| {
                                v.validate().map_err(str::to_owned)?;
                                v.normalize();
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "query-request" => serde_json::from_str::<QueryRequest>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "command-reply" => serde_json::from_str::<CommandReply>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "query-reply" => serde_json::from_str::<QueryReply>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "event" => serde_json::from_str::<Event>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "inference-request" => serde_json::from_str::<InferenceRequest>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "provider-event" => serde_json::from_str::<ProviderEvent>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "transcript-batch" => serde_json::from_str::<TranscriptBatch>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "session-command-request" => {
                            serde_json::from_str::<SessionCommandRequest>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    v.validate().map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "session-query-request" => {
                            serde_json::from_str::<SessionQueryRequest>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    v.validate().map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "transcript-page" => serde_json::from_str::<TranscriptPage>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "package-manifest" => serde_json::from_str::<PackageManifest>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "package-host-request" => serde_json::from_str::<HostRequest>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "package-request" => serde_json::from_str::<PackageRequest>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "package-notification" => {
                            serde_json::from_str::<PackageNotification>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    v.validate().map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "package-response" => serde_json::from_str::<RpcResponse>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "addon-command-request" => {
                            serde_json::from_str::<AddonCommandRequest>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string()))
                        }
                        "addon-command-reply" => {
                            serde_json::from_str::<AddonCommandReply>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    validate_domain_reply(&v).map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "session-command-reply" => {
                            serde_json::from_str::<SessionCommandReply>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    validate_domain_reply(&v).map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "inference-spec" => serde_json::from_str::<InferenceSpec>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "provider-codec-event" => {
                            serde_json::from_str::<ProviderCodecEvent>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    v.validate().map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "attempt-envelope" => serde_json::from_str::<AttemptEnvelope>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "route-decision" => serde_json::from_str::<RouteDecision>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string())),
                        "capability-snapshot" => {
                            serde_json::from_str::<CapabilitySnapshot>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string()))
                        }
                        "graph-read-request" => serde_json::from_str::<GraphReadRequest>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "resolved-view-set" => serde_json::from_str::<ResolvedViewSet>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "retrieval-request" => serde_json::from_str::<RetrievalRequest>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "retrieval-page" => serde_json::from_str::<RetrievalPage>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "index-command-request" => {
                            serde_json::from_str::<IndexCommandRequest>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string()))
                        }
                        "publication-request" => {
                            serde_json::from_str::<PublicationRequest>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    v.validate().map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "artifact-command-request" => {
                            serde_json::from_str::<ArtifactCommandRequest>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    v.validate().map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "artifact-command-reply" => {
                            serde_json::from_str::<ArtifactCommandReply>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    validate_domain_reply(&v).map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "artifact-query-request" => {
                            serde_json::from_str::<ArtifactQueryRequest>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    v.validate().map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "delivery-barrier" => serde_json::from_str::<DeliveryBarrier>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "git-command-request" => {
                            serde_json::from_str::<GitCommandRequest>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    v.validate().map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "delivery-snapshot" => serde_json::from_str::<DeliverySnapshot>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "release-command-request" => {
                            serde_json::from_str::<ReleaseCommandRequest>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    v.validate().map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "graph-page" => serde_json::from_str::<GraphPage>(&case.raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "artifact-deletion-request" => {
                            serde_json::from_str::<ArtifactDeletionRequest>(&case.raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string()))
                        }
                        _ => Err("unknown entry point".into()),
                    }
                };
                let valid = result.is_ok();
                let passed = valid == case.typed_valid;
                results.push(json!({"id":case.id,"entry":case.entry,"passed":passed,"typedValid":valid,"expectedSchemaValid":case.schema_valid,"normalized":result.as_ref().ok(),"error":result.err()}));
            }
            let passed = results.iter().all(|x| x["passed"] == true);
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"scope":"DTO shape/range checks only; no gateway, policy, storage, dispatch or platform proof","allPassed":passed,"cases":results})
                )?
            );
            if !passed {
                std::process::exit(1);
            }
        }
        _ => {
            return Err(
                "Usage: devworks-contract-probe emit DIRECTORY | check FIXTURES.json".into(),
            );
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
enum Scenario {
    Stream {
        id: String,
        events: Vec<ProviderEvent>,
        expected_accept: Vec<bool>,
        expected_ready: Vec<bool>,
    },
    Transcript {
        id: String,
        batches: Vec<TranscriptBatch>,
        expected: Vec<String>,
        expected_next: Vec<i64>,
    },
}
fn scenario_check(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let cases: Vec<Scenario> = serde_json::from_str(&fs::read_to_string(path)?)?;
    let mut results = Vec::new();
    for c in cases {
        match c {
            Scenario::Stream {
                id,
                events,
                expected_accept,
                expected_ready,
            } => {
                if events.len() != expected_accept.len() || events.len() != expected_ready.len() {
                    return Err("scenario length mismatch".into());
                }
                let mut model = StreamModel::default();
                let mut steps = Vec::new();
                for (i, e) in events.iter().enumerate() {
                    let accepted = model.accept(e).is_ok();
                    let ready = model.tools_ready();
                    steps.push(json!({"accepted":accepted,"ready":ready,"passed":accepted==expected_accept[i]&&ready==expected_ready[i]}));
                }
                results.push(json!({"id":id,"steps":steps}));
            }
            Scenario::Transcript {
                id,
                batches,
                expected,
                expected_next,
            } => {
                if batches.len() != expected.len() || batches.len() != expected_next.len() {
                    return Err("scenario length mismatch".into());
                }
                let mut model = TranscriptModel::default();
                let mut steps = Vec::new();
                for (i, b) in batches.iter().enumerate() {
                    let state = model.accept(b).unwrap_or("Rejected");
                    steps.push(json!({"result":state,"next":model.next_sequence(),"passed":state==expected[i]&&model.next_sequence()==expected_next[i]}));
                }
                results.push(json!({"id":id,"steps":steps}));
            }
        }
    }
    let passed = results.iter().all(|c| {
        c["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["passed"] == true)
    });
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"scope":"Native in-memory contract models only; no provider network, gateway, storage or effects","allPassed":passed,"scenarios":results})
        )?
    );
    if !passed {
        std::process::exit(1);
    }
    Ok(())
}

fn decode_domain<T: serde::de::DeserializeOwned + serde::Serialize>(
    raw: &str,
    check: impl FnOnce(&T) -> Result<(), String>,
) -> Result<serde_json::Value, String> {
    strict_json(raw)?;
    let v: T = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    check(&v)?;
    serde_json::to_value(v).map_err(|e| e.to_string())
}
fn strict_json(raw: &str) -> Result<(), String> {
    use serde::de::{Error, MapAccess, SeqAccess, Visitor};
    struct Strict;
    impl<'de> Visitor<'de> for Strict {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "JSON without duplicate members")
        }
        fn visit_bool<E: Error>(self, _: bool) -> Result<(), E> {
            Ok(())
        }
        fn visit_i64<E: Error>(self, _: i64) -> Result<(), E> {
            Ok(())
        }
        fn visit_u64<E: Error>(self, _: u64) -> Result<(), E> {
            Ok(())
        }
        fn visit_f64<E: Error>(self, _: f64) -> Result<(), E> {
            Ok(())
        }
        fn visit_str<E: Error>(self, _: &str) -> Result<(), E> {
            Ok(())
        }
        fn visit_unit<E: Error>(self) -> Result<(), E> {
            Ok(())
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<(), A::Error> {
            while a.next_element::<StrictValue>()?.is_some() {}
            Ok(())
        }
        fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<(), A::Error> {
            let mut seen = std::collections::HashSet::new();
            while let Some(key) = a.next_key::<String>()? {
                if !seen.insert(key) {
                    return Err(A::Error::custom("duplicate member"));
                }
                a.next_value::<StrictValue>()?;
            }
            Ok(())
        }
    }
    struct StrictValue;
    impl<'de> serde::Deserialize<'de> for StrictValue {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_any(Strict)?;
            Ok(Self)
        }
    }
    serde_json::from_str::<StrictValue>(raw)
        .map(|_| ())
        .map_err(|e| e.to_string())
}
