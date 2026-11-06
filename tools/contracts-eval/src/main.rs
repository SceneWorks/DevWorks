//! Emit candidate schemas or check structural DTO fixtures. No authority/engine execution.
use devworks_contract_probe::data_delivery::*;
use devworks_contract_probe::domain_generated::*;
use devworks_contract_probe::domain_support::Revisioned;
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
                let canonical = serde_json::to_value(schema)?;
                // One document per schema revision that changes this shape: the base document
                // is revision 1; `<name>.rN` adds the members introduced at N (`x-since`).
                let mut revisions = vec![1];
                revisions.extend(
                    (2..=revision::latest(&canonical))
                        .filter(|r| revision::introduces(&canonical, *r)),
                );
                for r in revisions {
                    let file = if r == 1 {
                        name.to_owned()
                    } else {
                        format!("{name}.r{r}")
                    };
                    let mut value = revision::at(&canonical, r);
                    if ENVELOPES.contains(&name) {
                        revision::envelope_member(&mut value, r);
                    }
                    value["$id"] = json!(format!("urn:devworks:architecture:1.0-draft.1:{file}"));
                    value["$comment"] = json!(
                        "Architecture candidate only. Schema validation is followed by typed range, cross-field and authority validation. Not a public v1 SDK."
                    );
                    fs::write(
                        dir.join(format!("{file}.schema.json")),
                        serde_json::to_string_pretty(&value)? + "\n",
                    )?;
                }
            }
        }
        Some("check") => {
            let cases: Vec<Fixture> = serde_json::from_str(&fs::read_to_string(&args[2])?)?;
            let mut results = Vec::new();
            for case in cases {
                let (entry, doc) = revision::entry(&case.entry);
                let negotiated = negotiate(entry, doc, &case.raw);
                let result = if case.raw.len() > 1024 * 1024 {
                    Err("frame too large".into())
                } else if let Err(e) = &negotiated {
                    Err(e.clone())
                } else {
                    let (raw, revision) = negotiated.clone().unwrap();
                    match entry {
                        "domain-request" => decode_domain::<DomainRequest>(&raw, |v| {
                            at_revision(v, revision)?;
                            v.validate()
                        }),
                        "domain-query-reply" => decode_domain::<DomainQueryReply>(&raw, |v| {
                            if let DomainQueryReply::Result { result, .. } = v {
                                result.check_revision(revision)?;
                            }
                            v.validate()
                        }),
                        "domain-command-reply" => decode_domain::<DomainCommandReply>(&raw, |v| {
                            if let CommandReply::Recorded { receipt, .. } = v {
                                if let DomainDecision::Applied { result }
                                | DomainDecision::NoChange { result } = &receipt.decision
                                {
                                    result.check_revision(revision)?;
                                }
                            }
                            validate_command_reply(v)
                        }),
                        "operation-result" => decode_domain::<OperationResult>(&raw, |v| {
                            at_revision(v, revision)?;
                            // A union alternative introduced later (the command aggregate of an
                            // event) cannot be cleared from a value; its revision refuses it.
                            if let OperationResult::HistoryEvents { value } = v {
                                for event in &value.items.0 {
                                    event.validate_revision(revision)?;
                                    event.validate()?;
                                }
                            }
                            devworks_contract_probe::domain_support::Check::check(v)
                        }),
                        "operation-completion" => decode_domain::<OperationCompletion>(&raw, |v| {
                            if let OperationCompletion::Succeeded { result, .. } = v {
                                result.check_revision(revision)?;
                            }
                            v.validate()
                        }),
                        "command-request" => serde_json::from_str::<CommandRequest>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|mut v| {
                                v.validate().map_err(str::to_owned)?;
                                v.normalize();
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "query-request" => serde_json::from_str::<QueryRequest>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                at_revision(&v, revision)?;
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "command-reply" => serde_json::from_str::<CommandReply>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "query-reply" => serde_json::from_str::<QueryReply>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "event" => serde_json::from_str::<Event>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "inference-request" => serde_json::from_str::<InferenceRequest>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "provider-event" => serde_json::from_str::<ProviderEvent>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "transcript-batch" => serde_json::from_str::<TranscriptBatch>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "session-command-request" => {
                            serde_json::from_str::<SessionCommandRequest>(&raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    v.validate().map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "session-query-request" => {
                            serde_json::from_str::<SessionQueryRequest>(&raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    v.validate().map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "transcript-page" => serde_json::from_str::<TranscriptPage>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "package-manifest" => serde_json::from_str::<PackageManifest>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "package-host-request" => serde_json::from_str::<HostRequest>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "package-request" => serde_json::from_str::<PackageRequest>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "package-notification" => serde_json::from_str::<PackageNotification>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "package-response" => serde_json::from_str::<RpcResponse>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "addon-command-request" => {
                            serde_json::from_str::<AddonCommandRequest>(&raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string()))
                        }
                        "addon-command-reply" => serde_json::from_str::<AddonCommandReply>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                validate_domain_reply(&v).map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "session-command-reply" => {
                            serde_json::from_str::<SessionCommandReply>(&raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    validate_domain_reply(&v).map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "inference-spec" => serde_json::from_str::<InferenceSpec>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "provider-codec-event" => serde_json::from_str::<ProviderCodecEvent>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "attempt-envelope" => serde_json::from_str::<AttemptEnvelope>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "route-decision" => serde_json::from_str::<RouteDecision>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string())),
                        "capability-snapshot" => serde_json::from_str::<CapabilitySnapshot>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string())),
                        "graph-read-request" => serde_json::from_str::<GraphReadRequest>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "resolved-view-set" => serde_json::from_str::<ResolvedViewSet>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "retrieval-request" => serde_json::from_str::<RetrievalRequest>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "retrieval-page" => serde_json::from_str::<RetrievalPage>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "index-command-request" => {
                            serde_json::from_str::<IndexCommandRequest>(&raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    at_revision(&v, revision)?;
                                    v.validate().map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "publication-request" => serde_json::from_str::<PublicationRequest>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "artifact-command-request" => {
                            serde_json::from_str::<ArtifactCommandRequest>(&raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    at_revision(&v, revision)?;
                                    v.validate().map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "artifact-command-reply" => {
                            serde_json::from_str::<ArtifactCommandReply>(&raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    validate_domain_reply(&v).map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "artifact-query-request" => {
                            serde_json::from_str::<ArtifactQueryRequest>(&raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    at_revision(&v, revision)?;
                                    v.validate().map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "delivery-barrier" => serde_json::from_str::<DeliveryBarrier>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "git-command-request" => serde_json::from_str::<GitCommandRequest>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|mut v| {
                                at_revision(&v, revision)?;
                                v.validate().map_err(str::to_owned)?;
                                v.normalize();
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "delivery-snapshot" => serde_json::from_str::<DeliverySnapshot>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "release-command-request" => {
                            serde_json::from_str::<ReleaseCommandRequest>(&raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| {
                                    v.validate().map_err(str::to_owned)?;
                                    serde_json::to_value(v).map_err(|e| e.to_string())
                                })
                        }
                        "graph-page" => serde_json::from_str::<GraphPage>(&raw)
                            .map_err(|e| e.to_string())
                            .and_then(|v| {
                                v.validate().map_err(str::to_owned)?;
                                serde_json::to_value(v).map_err(|e| e.to_string())
                            }),
                        "artifact-deletion-request" => {
                            serde_json::from_str::<ArtifactDeletionRequest>(&raw)
                                .map_err(|e| e.to_string())
                                .and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string()))
                        }
                        _ => Err("unknown entry point".into()),
                    }
                };
                // The normalized envelope keeps the revision it negotiated.
                let result = match (result, negotiated) {
                    (Ok(mut v), Ok((_, revision)))
                        if ENVELOPES.contains(&entry)
                            && case.raw.contains("\"schemaRevision\"") =>
                    {
                        v["schemaRevision"] = json!(revision);
                        Ok(v)
                    }
                    (r, _) => r,
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

/// Operation request envelopes: they carry the optional negotiated `schemaRevision` member.
const ENVELOPES: [&str; 14] = [
    "domain-request",
    "command-request",
    "query-request",
    "graph-read-request",
    "artifact-command-request",
    "artifact-query-request",
    "artifact-deletion-request",
    "git-command-request",
    "index-command-request",
    "release-command-request",
    "retrieval-request",
    "session-command-request",
    "session-query-request",
    "addon-command-request",
];

/// Separates a request envelope's negotiated `schemaRevision` (absent means 1) from the frame the
/// typed DTO decodes. A revision document `<entry>.rN` describes requests at revision N or later.
/// Other entries are typed at their document's revision.
fn negotiate(entry: &str, doc: u32, raw: &str) -> Result<(String, u32), String> {
    if !ENVELOPES.contains(&entry) {
        return Ok((raw.to_owned(), doc));
    }
    let Ok(serde_json::Value::Object(mut map)) = serde_json::from_str::<serde_json::Value>(raw)
    else {
        return Ok((raw.to_owned(), 1));
    };
    let Some(member) = map.remove("schemaRevision") else {
        if doc > 1 {
            return Err(format!("schemaRevision {doc} or later is required"));
        }
        return Ok((raw.to_owned(), 1));
    };
    strict_json(raw)?;
    let revision = member
        .as_u64()
        .filter(|r| (1..=u64::from(u16::MAX)).contains(r))
        .ok_or("schemaRevision must be an integer from 1 to 65535")? as u32;
    if revision < doc {
        return Err(format!("schemaRevision {revision} precedes revision {doc}"));
    }
    Ok((serde_json::Value::Object(map).to_string(), revision))
}

/// Strict revision reader: a member introduced after `revision` is refused, not discarded.
fn at_revision<T>(v: &T, revision: u32) -> Result<(), String>
where
    T: devworks_contract_probe::domain_support::Revisioned + Clone + serde::Serialize,
{
    v.check_revision(revision)?;
    let mut at = v.clone();
    at.retain_revision(revision);
    if serde_json::to_value(&at).map_err(|e| e.to_string())?
        != serde_json::to_value(v).map_err(|e| e.to_string())?
    {
        return Err(format!(
            "a member introduced after schema revision {revision} is present"
        ));
    }
    Ok(())
}

/// JSON Schema documents per operation schema revision.
mod revision {
    use serde_json::{Value, json};
    use std::collections::BTreeSet;

    /// `name.rN` names revision document N of `name`; a plain name is revision 1.
    pub fn entry(entry: &str) -> (&str, u32) {
        if let Some((base, r)) = entry.rsplit_once(".r")
            && let Ok(r) = r.parse::<u32>()
            && r >= 2
        {
            return (base, r);
        }
        (entry, 1)
    }

    fn since(v: &Value) -> Option<u64> {
        v.get("x-since").and_then(Value::as_u64)
    }
    fn walk(v: &Value, f: &mut impl FnMut(&Value)) {
        f(v);
        match v {
            Value::Object(m) => m.values().for_each(|x| walk(x, f)),
            Value::Array(a) => a.iter().for_each(|x| walk(x, f)),
            _ => {}
        }
    }
    /// The latest revision any member of the document was introduced at.
    pub fn latest(v: &Value) -> u32 {
        let mut max = 1;
        walk(v, &mut |x| max = max.max(since(x).unwrap_or(1) as u32));
        max
    }
    /// Whether some member was introduced exactly at `revision`.
    pub fn introduces(v: &Value, revision: u32) -> bool {
        let mut found = false;
        walk(v, &mut |x| found |= since(x) == Some(u64::from(revision)));
        found
    }
    /// Marks a union that lost alternatives at the revision being produced.
    const PRUNED: &str = "x-pruned";
    fn later(v: &Value, revision: u32) -> bool {
        v.get("x-since")
            .and_then(Value::as_u64)
            .is_some_and(|s| s > u64::from(revision))
            || v.get("x-until")
                .and_then(Value::as_u64)
                .is_some_and(|end| u64::from(revision) >= end)
    }
    /// Removes the members and union alternatives introduced after `revision`.
    fn strip(v: &mut Value, revision: u32) {
        match v {
            Value::Object(m) => {
                let mut gone = Vec::new();
                if let Some(Value::Object(props)) = m.get_mut("properties") {
                    gone = props
                        .iter()
                        .filter(|(_, p)| later(p, revision))
                        .map(|(k, _)| k.clone())
                        .collect();
                    props.retain(|k, _| !gone.contains(k));
                }
                if let Some(Value::Array(required)) = m.get_mut("required") {
                    required.retain(|r| !gone.iter().any(|g| r == g));
                }
                let mut pruned = false;
                for key in ["oneOf", "anyOf"] {
                    if let Some(Value::Array(alts)) = m.get_mut(key) {
                        let before = alts.len();
                        alts.retain(|a| !later(a, revision));
                        pruned |= alts.len() < before;
                    }
                }
                if pruned {
                    m.insert(PRUNED.into(), Value::Bool(true));
                }
                m.values_mut().for_each(|x| strip(x, revision));
            }
            Value::Array(a) => a.iter_mut().for_each(|x| strip(x, revision)),
            _ => {}
        }
    }
    /// A union that lost alternatives and is left with one is that alternative: alone it
    /// replaces the union; an alternative that only lists the required members (an exactly-one
    /// member choice) folds back into its object's required list.
    fn collapse(v: &mut Value) {
        if let Value::Object(m) = v
            && m.remove(PRUNED).is_some()
        {
            for key in ["oneOf", "anyOf"] {
                let single = m
                    .get(key)
                    .and_then(Value::as_array)
                    .filter(|alts| alts.len() == 1)
                    .map(|alts| alts[0].clone());
                let Some(alt) = single else { continue };
                if m.len() == 1 {
                    *v = alt;
                    return collapse(v);
                }
                if alt
                    .as_object()
                    .is_some_and(|a| a.len() == 1 && a.contains_key("required"))
                {
                    m.remove(key);
                    m.insert("required".into(), alt["required"].clone());
                }
            }
        }
        match v {
            Value::Object(m) => m.values_mut().for_each(collapse),
            Value::Array(a) => a.iter_mut().for_each(collapse),
            _ => {}
        }
    }
    fn retarget(v: &mut Value, from: &str, to: &str) {
        match v {
            Value::Object(m) => {
                if m.get("$ref").and_then(Value::as_str) == Some(from) {
                    m.insert("$ref".into(), Value::String(to.to_owned()));
                }
                m.values_mut().for_each(|x| retarget(x, from, to));
            }
            Value::Array(a) => a.iter_mut().for_each(|x| retarget(x, from, to)),
            _ => {}
        }
    }
    /// The document at `revision`: members and union alternatives introduced later, and the
    /// definitions only they reference, are removed. A definition annotated
    /// `x-earlier: {name, before}` is, below `before`, the recorded definition `name` (its later
    /// description is dropped): it takes that name, or merges into that definition, which it must
    /// then equal exactly. Revision 1 of a document without gated members is unchanged.
    fn revision_document(canonical: &Value, revision: u32) -> Value {
        let mut v = canonical.clone();
        strip(&mut v, revision);
        let mut renames = Vec::new();
        if let Some(Value::Object(defs)) = v.get_mut("$defs") {
            for (name, d) in defs.iter_mut() {
                let Some(m) = d.as_object_mut() else { continue };
                if let Some(e) = m.remove("x-earlier")
                    && e["before"]
                        .as_u64()
                        .is_some_and(|b| u64::from(revision) < b)
                    && let Some(to) = e["name"].as_str()
                {
                    m.remove("description");
                    renames.push((name.clone(), to.to_owned()));
                }
            }
        }
        collapse(&mut v);
        if let Some(Value::Object(defs)) = v.get_mut("$defs") {
            for (from, to) in &renames {
                let reference = format!("#/$defs/{to}");
                if defs
                    .get(from)
                    .and_then(|d| d.get("$ref"))
                    .and_then(Value::as_str)
                    == Some(reference.as_str())
                    && let Some(original) = defs.get(to).cloned()
                {
                    defs.insert(from.clone(), original);
                }
            }
        }
        // Definitions named after a renamed one (`List_<name>_0_500`) take the recorded name too.
        if let Some(Value::Object(defs)) = v.get("$defs") {
            let primary = renames.clone();
            for name in defs.keys() {
                for (from, to) in &primary {
                    let segments: Vec<&str> = name.split('_').collect();
                    if name != from && segments.contains(&from.as_str()) {
                        let renamed: Vec<&str> = segments
                            .iter()
                            .map(|s| if s == from { to.as_str() } else { s })
                            .collect();
                        renames.push((name.clone(), renamed.join("_")));
                    }
                }
            }
        }
        // A generic family name (`Page`, which schemars numbers `Page2`, `Page3`, ... by first
        // use) is the member this definition equals once the other renames apply.
        if let Some(Value::Object(defs)) = v.get("$defs") {
            let primary = renames.clone();
            for (from, to) in renames.iter_mut() {
                let mut def = defs[from.as_str()].clone();
                for (f, t) in &primary {
                    retarget(&mut def, &format!("#/$defs/{f}"), &format!("#/$defs/{t}"));
                }
                if defs.get(to.as_str()).is_some_and(|e| *e == def) {
                    continue;
                }
                let member = defs.iter().find(|(name, existing)| {
                    name.strip_prefix(to.as_str())
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                        && **existing == def
                });
                if let Some((name, _)) = member {
                    *to = name.clone();
                }
            }
        }
        for (from, to) in &renames {
            retarget(&mut v, &format!("#/$defs/{from}"), &format!("#/$defs/{to}"));
        }
        if let Some(Value::Object(defs)) = v.get_mut("$defs") {
            for (from, to) in &renames {
                let def = defs.remove(from).expect("renamed definition");
                match defs.get(to) {
                    Some(existing) => assert_eq!(
                        existing, &def,
                        "{from} at revision {revision} must equal the recorded {to}"
                    ),
                    None => {
                        defs.insert(to.clone(), def);
                    }
                }
            }
        }
        prune_definitions(v)
    }
    fn refs(v: &Value, out: &mut BTreeSet<String>) {
        match v {
            Value::Object(m) => {
                if let Some(name) = m
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|r| r.strip_prefix("#/$defs/"))
                {
                    out.insert(name.to_owned());
                }
                m.values().for_each(|x| refs(x, out));
            }
            Value::Array(a) => a.iter().for_each(|x| refs(x, out)),
            _ => {}
        }
    }
    /// Keeps only the definitions the document still references (transitively).
    fn prune_definitions(mut v: Value) -> Value {
        let Some(Value::Object(defs)) = v.as_object_mut().and_then(|m| m.remove("$defs")) else {
            return v;
        };
        let mut live = BTreeSet::new();
        refs(&v, &mut live);
        let mut frontier: Vec<String> = live.iter().cloned().collect();
        while let Some(name) = frontier.pop() {
            let mut next = BTreeSet::new();
            if let Some(d) = defs.get(&name) {
                refs(d, &mut next);
            }
            frontier.extend(next.into_iter().filter(|n| live.insert(n.clone())));
        }
        let kept: serde_json::Map<String, Value> =
            defs.into_iter().filter(|(k, _)| live.contains(k)).collect();
        if !kept.is_empty() {
            v["$defs"] = Value::Object(kept);
        }
        v
    }
    /// The document at `revision` (see `revision_document`).
    pub fn at(canonical: &Value, revision: u32) -> Value {
        revision_document(canonical, revision)
    }
    /// Adds the negotiated `schemaRevision` member to each envelope object of a request
    /// document. Absent means 1; a revision-N document requires N or later.
    pub fn envelope_member(v: &mut Value, revision: u32) {
        let member = json!({
            "description": "Negotiated operation schema revision. Absent means 1. Only revisions the selected operation registers are admitted; any other is UnsupportedSchema before authority.",
            "type": "integer",
            "minimum": revision,
            "maximum": 65535
        });
        let mut envelopes: Vec<&mut Value> = Vec::new();
        if v.pointer("/properties/protocol").is_some() {
            envelopes.push(v);
        } else if let Some(Value::Array(variants)) = v.get_mut("oneOf") {
            envelopes.extend(
                variants
                    .iter_mut()
                    .filter(|x| x.pointer("/properties/protocol").is_some()),
            );
        }
        for e in envelopes {
            e["properties"]["schemaRevision"] = member.clone();
            if revision > 1
                && let Some(Value::Array(req)) = e.get_mut("required")
            {
                req.push(json!("schemaRevision"));
            }
        }
    }
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
