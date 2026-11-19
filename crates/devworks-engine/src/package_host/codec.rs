//! Package codecs run inside the native router's attempt. The router keeps the HTTP connection,
//! authentication, selection and accounting. A codec sees the admitted request and response data.
use super::{
    InvocationHandle, InvocationKind, InvokeOutcome, InvokeRequest, PackageHost,
    contributions::scope_of,
};
use devworks_core::context::InvocationContext;
use devworks_core::ports::packages::Inspection;
use devworks_core::ports::storage::{Cancellation, StorageError};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::providers::{
    PreparedRequest,
    decode::{
        DecodedFrame, DecodedOutcome, DecodedToolCall, DecodedUsage, NativeObservation,
        NativeReplay, ReportedCost,
    },
};
use devworks_extension_protocol::codec::{CodecBody, RawUsageObservation};
use devworks_extension_protocol::package::{
    Contribution, ContributionEvent, ContributionMeta, InvocationOutcome,
};
use devworks_extension_protocol::scalars as wire;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_CODEC_BYTES: u64 = super::artifact_io::MAX_ARTIFACT as u64;

pub(crate) struct PackageCodec {
    host: Arc<PackageHost>,
    principal: InvocationContext,
    inspection: Inspection,
    meta: ContributionMeta,
    sequence: u64,
    calls: BTreeSet<String>,
}

impl PackageHost {
    pub(crate) fn select_codec(
        self: &Arc<Self>,
        principal: &InvocationContext,
        prepared: &PreparedRequest,
    ) -> Result<Option<PackageCodec>, StorageError> {
        let connection = &prepared.identity.binding.connection_id;
        let scope = format!("provider:{}:{:?}", connection.as_str(), prepared.profile);
        let installed = self
            .active_contributions(principal, "Provider")
            .map_err(invalid)?;
        let mut selected = None;
        for inspection in installed
            .into_iter()
            .filter(|version| version.access.contains(&scope))
        {
            for entry in &inspection.contributions {
                let Some(Contribution::Provider {
                    meta,
                    profiles,
                    endpoint_ids,
                }) = entry
                    .verified_descriptor()
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                else {
                    continue;
                };
                if !endpoint_ids
                    .iter()
                    .any(|id| id.as_str() == connection.as_str())
                    || !profiles
                        .iter()
                        .any(|profile| format!("{profile:?}") == format!("{:?}", prepared.profile))
                {
                    continue;
                }
                if selected.is_some() {
                    return Err(invalid(
                        "the routed endpoint selects multiple package codecs",
                    ));
                }
                selected = Some(PackageCodec {
                    host: self.clone(),
                    principal: principal.clone(),
                    inspection: inspection.clone(),
                    meta,
                    sequence: 0,
                    calls: BTreeSet::new(),
                });
            }
        }
        Ok(selected)
    }
}

impl PackageCodec {
    pub(crate) fn current(&self) -> Result<(), StorageError> {
        self.host
            .active_contributions(&self.principal, "Provider")
            .map_err(invalid)?
            .iter()
            .any(|version| {
                version.package_id == self.inspection.package_id
                    && version.manifest_digest == self.inspection.manifest_digest
            })
            .then_some(())
            .ok_or(StorageError::ScopeDenied)
    }

    fn invoke(
        &self,
        prepared: &PreparedRequest,
        input: Value,
        cancellation: &Cancellation,
    ) -> Result<(InvocationHandle, Vec<wire::ArtifactRef>), StorageError> {
        self.current()?;
        if cancellation.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let request = json!({"requestId":prepared.identity.request_id,"attemptId":prepared.identity.attempt_id,
            "binding":prepared.identity.binding,"upstreamModel":prepared.identity.upstream_model,
            "payload":input});
        let bytes = serde_json::to_vec(&request).map_err(|error| invalid(error.to_string()))?;
        if bytes.len() as u64 > MAX_CODEC_BYTES {
            return Err(invalid("the codec input exceeds its bound"));
        }
        let expected = self
            .host
            .contribution_schema(&self.inspection, &self.meta.input_schema)
            .map_err(invalid)?;
        self.host
            .service
            .validator()
            .validate(&expected, &request)
            .map_err(|_| invalid("the codec input does not match its exact schema"))?;
        let input = self
            .host
            .stage_contribution_input(
                &self.principal,
                (
                    &self.inspection.package_id,
                    &self.inspection.manifest_digest,
                ),
                &bytes,
                json!({"kind":"ProviderCodecInput","attemptId":prepared.identity.attempt_id,
                "contributionId":self.meta.id,"manifestDigest":self.inspection.manifest_digest}),
            )
            .map_err(invalid)?;
        let budget = Duration::from_secs(30);
        let handle = self
            .host
            .invoke(InvokeRequest {
                package_id: self.inspection.package_id.clone(),
                contribution_id: self.meta.id.as_str().to_owned(),
                scope: scope_of(&self.principal).map_err(invalid)?,
                input: serde_json::to_value(input)
                    .and_then(serde_json::from_value)
                    .map_err(|error| invalid(error.to_string()))?,
                operation_id: prepared.identity.attempt_id.0.clone(),
                deadline: budget,
                allowed_operations: Vec::new(),
                kind: InvocationKind::ProviderCodec,
                principal: Some(self.principal.clone()),
                expected_digest: Some(self.inspection.manifest_digest.clone()),
            })
            .map_err(|error| invalid(format!("the selected codec is unavailable: {error:?}")))?;
        let limit = Instant::now() + budget;
        loop {
            if cancellation.is_cancelled() || Instant::now() >= limit {
                handle.cancel();
                return Err(StorageError::Cancelled);
            }
            if let Some(outcome) = handle.wait(Duration::from_millis(150)) {
                let InvokeOutcome::Finished(InvocationOutcome::Completed { artifacts }) = outcome
                else {
                    return Err(invalid("the selected codec invocation did not complete"));
                };
                self.current()?;
                return Ok((handle, artifacts));
            }
        }
    }

    pub(crate) fn encode(
        &self,
        prepared: &PreparedRequest,
        cancellation: &Cancellation,
    ) -> Result<Vec<u8>, StorageError> {
        let body = devworks_core::framing::parse(&prepared.body)
            .map_err(|_| invalid("invalid prepared codec input"))?;
        let (handle, artifacts) =
            self.invoke(prepared, json!({"kind":"Encode","body":body}), cancellation)?;
        let [reference] = artifacts.as_slice() else {
            return Err(invalid("codec encode must publish one result"));
        };
        let result = self.output(&handle, reference)?.0;
        self.validate_output(&result)?;
        let Some(encoded) = result
            .as_object()
            .filter(|object| object.len() == 1)
            .and_then(|object| object.get("body"))
            .and_then(Value::as_object)
        else {
            return Err(invalid("the codec did not return a request body"));
        };
        if !body.as_object().is_some_and(|admitted| {
            admitted
                .iter()
                .all(|(key, value)| encoded.get(key) == Some(value))
        }) {
            return Err(invalid(
                "the codec changed the already admitted request semantics",
            ));
        }
        serde_json::to_vec(encoded).map_err(|error| invalid(error.to_string()))
    }

    pub(crate) fn decode(
        &mut self,
        prepared: &PreparedRequest,
        response: Value,
        end_of_input: bool,
        cancellation: &Cancellation,
    ) -> Result<Vec<DecodedFrame>, StorageError> {
        let (handle, _) = self.invoke(
            prepared,
            json!({"kind":"Decode","response":response,"endOfInput":end_of_input,
                "nextSequence":(self.sequence+1).to_string()}),
            cancellation,
        )?;
        let (events, dropped) = handle.events();
        if dropped != 0 {
            return Err(invalid("the codec exceeded its bounded event stream"));
        }
        let mut normalized = Vec::with_capacity(events.len());
        let mut sequence = self.sequence;
        let mut terminal = false;
        let binding = serde_json::to_value(&prepared.identity.binding)
            .map_err(|error| invalid(error.to_string()))?;
        for event in &events {
            let ContributionEvent::Provider { event } = event else {
                return Err(invalid("the codec produced a foreign event kind"));
            };
            sequence += 1;
            if terminal
                || event.request_id.as_str() != prepared.identity.request_id.as_str()
                || event.attempt_id.0.as_str() != prepared.identity.attempt_id.0.as_str()
                || number(event.sequence.clone().into())? != sequence
            {
                return Err(invalid(
                    "the codec event is stale, out of order or belongs to another attempt",
                ));
            }
            normalized.push(match &event.event {
                CodecBody::TextDelta { block_index, text } => {
                    if prepared.embedding_dimensions.is_some() {
                        return Err(invalid("text in an embedding response"));
                    }
                    DecodedFrame::TextDelta {
                        block_index: number(block_index.clone().into())?,
                        content_index: 0,
                        text: text.clone(),
                    }
                }
                CodecBody::ToolCallComplete { tool_call } => {
                    let schema = serde_json::to_value(&tool_call.input_schema)
                        .map_err(|error| invalid(error.to_string()))?;
                    let tool = prepared
                        .tools
                        .iter()
                        .find(|tool| {
                            tool.capability_id.as_str() == tool_call.capability_id.as_str()
                                && serde_json::to_value(&tool.input_schema).ok().as_ref()
                                    == Some(&schema)
                        })
                        .ok_or_else(|| {
                            invalid("the codec named a tool outside the selected request")
                        })?;
                    let ordinal = number(tool_call.call.ordinal.clone().into())?;
                    if !self.calls.insert(tool_call.call.native_call_id.clone()) {
                        return Err(invalid("the codec repeated a tool call"));
                    }
                    let (arguments, arguments_raw) = self.output(&handle, &tool_call.arguments)?;
                    self.host
                        .service
                        .validator()
                        .validate(&tool.schema, &arguments)
                        .map_err(|_| {
                            invalid("the codec tool arguments fail the selected schema")
                        })?;
                    let (native_item, _) = self.output(&handle, &tool_call.native_item)?;
                    DecodedFrame::ToolCallComplete {
                        call: DecodedToolCall {
                            native_call_id: tool_call.call.native_call_id.clone(),
                            native_item_id: native_item["id"].as_str().map(str::to_owned),
                            ordinal,
                            content_position: None,
                            name: tool.wire_name.clone(),
                            arguments_raw,
                            arguments,
                            native_item,
                        },
                    }
                }
                CodecBody::NativeContinuation {
                    binding: returned,
                    artifact,
                } => {
                    if serde_json::to_value(returned).ok().as_ref() != Some(&binding) {
                        return Err(invalid(
                            "the codec returned foreign provider continuation state",
                        ));
                    }
                    let (value, raw_json) = self.output(&handle, artifact)?;
                    DecodedFrame::Native {
                        observation: NativeObservation {
                            ordinal: None,
                            content_position: None,
                            kind: "package-codec".into(),
                            continuation: Some(value.clone()),
                            value,
                            raw_json,
                            replay: NativeReplay::Supported,
                        },
                    }
                }
                CodecBody::Embedding { index, artifact } => {
                    let dimensions = prepared
                        .embedding_dimensions
                        .ok_or_else(|| invalid("unexpected codec embedding"))?;
                    let values = self.output(&handle, artifact)?.0;
                    let vector = values["vector"]
                        .as_array()
                        .filter(|v| v.len() == dimensions as usize)
                        .ok_or_else(|| invalid("the codec vector has the wrong dimension"))?
                        .iter()
                        .map(|value| {
                            value
                                .as_f64()
                                .map(|n| n as f32)
                                .filter(|n| n.is_finite())
                                .ok_or_else(|| {
                                    invalid("the codec vector contains an invalid value")
                                })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    DecodedFrame::Embedding {
                        index: number(index.clone().into())?,
                        vector,
                    }
                }
                CodecBody::Usage { observation } => DecodedFrame::Usage {
                    observation: usage(observation)?,
                },
                CodecBody::Terminal {
                    outcome,
                    native_stop_reason,
                } => {
                    terminal = true;
                    DecodedFrame::Terminal {
                        outcome: match outcome {
                            wire::TurnOutcome::Completed => DecodedOutcome::Completed,
                            wire::TurnOutcome::ToolCallsReady => DecodedOutcome::ToolCallsReady,
                            wire::TurnOutcome::Incomplete => DecodedOutcome::Incomplete,
                            wire::TurnOutcome::Cancelled => DecodedOutcome::Cancelled,
                            wire::TurnOutcome::Failed => DecodedOutcome::Failed,
                            wire::TurnOutcome::OutcomeUnknown => DecodedOutcome::OutcomeUnknown,
                        },
                        reason: native_stop_reason.clone(),
                    }
                }
                CodecBody::Heartbeat => DecodedFrame::Heartbeat,
            });
        }
        if end_of_input && !terminal {
            return Err(invalid("the codec produced no terminal status"));
        }
        self.validate_output(&json!({"events":events}))?;
        self.sequence = sequence;
        Ok(normalized)
    }

    fn validate_output(&self, value: &Value) -> Result<(), StorageError> {
        let schema = self
            .host
            .contribution_schema(&self.inspection, &self.meta.output_schema)
            .map_err(invalid)?;
        self.host
            .service
            .validator()
            .validate(&schema, value)
            .map_err(|_| invalid("the codec output does not match its exact schema"))
    }

    fn output(
        &self,
        handle: &InvocationHandle,
        reference: &wire::ArtifactRef,
    ) -> Result<(Value, String), StorageError> {
        if !handle.produced(reference) {
            return Err(invalid(
                "the codec named an artifact outside its invocation",
            ));
        }
        let reference: ArtifactRef = serde_json::to_value(reference)
            .and_then(serde_json::from_value)
            .map_err(|error| invalid(error.to_string()))?;
        let ports = self
            .host
            .broker
            .get()
            .ok_or_else(|| invalid("codec artifacts are unavailable"))?;
        let (bytes, _) = ports
            .artifacts
            .read_exact(
                &*self.host.store,
                &self.principal,
                &reference,
                MAX_CODEC_BYTES,
            )
            .map_err(|_| invalid("the exact codec output is unreadable"))?;
        let value =
            devworks_core::framing::parse(&bytes).map_err(|error| invalid(error.to_string()))?;
        let raw = String::from_utf8(bytes).map_err(|_| invalid("codec output is not UTF-8"))?;
        Ok((value, raw))
    }
}

fn number(value: String) -> Result<u64, StorageError> {
    value.parse().map_err(|_| invalid("invalid codec counter"))
}

fn usage(value: &RawUsageObservation) -> Result<DecodedUsage, StorageError> {
    let count =
        |value: &Option<wire::Counter>| value.clone().map(|value| number(value.into())).transpose();
    Ok(DecodedUsage {
        semantics: devworks_core::providers::meter::ObservationSemantics::Cumulative,
        service_tier: None,
        processing_region: None,
        input_tokens: count(&value.input_tokens)?,
        output_tokens: count(&value.output_tokens)?,
        total_tokens: None,
        cached_read_tokens: count(&value.cached_read_tokens)?,
        cache_write_tokens: count(&value.cache_write_tokens)?,
        cache_writes: Vec::new(),
        reasoning_tokens: None,
        input_audio_tokens: None,
        output_audio_tokens: None,
        input_semantics: None,
        cost: value.reported_cost.as_ref().map(|cost| ReportedCost {
            amount: cost.amount.clone(),
            currency: Some(cost.currency.as_str().into()),
            raw_lexeme: cost.amount.clone(),
        }),
        other_costs: Default::default(),
        raw_json: serde_json::to_string(value).map_err(|error| invalid(error.to_string()))?,
        validation_errors: Vec::new(),
    })
}

fn invalid(message: impl Into<String>) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
