//! One transport owns one accounting attempt; credentials are obtained only at actual dispatch.
use devworks_core::{
    ports::{
        http::HttpRequest,
        provider_http::{HttpStream, SendPhase, StreamLimits, StreamingHttp, TransportFailure},
        secrets::Secret,
        storage::*,
    },
    protocol::provider::EndpointProfile,
    providers::{
        PreparedRequest, ResponseMode,
        decode::{DecodedFrame, DecodedOutcome, Decoder},
    },
    services::{
        router::connections::{AccessCredential, ConnectionService},
        usage::{
            accounting::{self, AttemptEnd},
            host::{InferenceFrame, OwnedInferenceSource},
            meter::{MeasuredQuantity, ObservationSemantics, Quantities},
        },
    },
};
use std::{collections::VecDeque, sync::Arc};

pub struct NativeProviderSource {
    prepared: PreparedRequest,
    connections: Arc<ConnectionService>,
    http: Arc<dyn StreamingHttp>,
    limits: StreamLimits,
    endpoint: String,
    credential_revision: i64,
    stream: Option<Box<dyn HttpStream>>,
    decoder: Decoder,
    sse: Option<devworks_core::providers::sse::SseDecoder>,
    aggregate: Vec<u8>,
    pending: VecDeque<InferenceFrame>,
    ended: bool,
    terminal: Option<AttemptEnd>,
    completed_items: Vec<DecodedFrame>,
    usage_sequence: i64,
    provider_model: Option<String>,
    status: u16,
    rejected: Option<Secret>,
    start_failure: AttemptEnd,
    send_gate: Option<Box<dyn Fn() -> Result<(), StorageError> + Send>>,
}
impl NativeProviderSource {
    pub fn new(
        prepared: PreparedRequest,
        connections: Arc<ConnectionService>,
        http: Arc<dyn StreamingHttp>,
        limits: StreamLimits,
        endpoint: String,
        credential_revision: i64,
    ) -> Self {
        let decoder = Decoder::new(prepared.profile.clone());
        Self {
            prepared,
            connections,
            http,
            limits,
            endpoint,
            credential_revision,
            stream: None,
            decoder,
            sse: None,
            aggregate: Vec::new(),
            pending: VecDeque::new(),
            ended: false,
            terminal: None,
            completed_items: Vec::new(),
            usage_sequence: 0,
            provider_model: None,
            status: 0,
            rejected: None,
            start_failure: AttemptEnd::Failed,
            send_gate: None,
        }
    }
    pub fn with_send_gate(
        mut self,
        gate: Box<dyn Fn() -> Result<(), StorageError> + Send>,
    ) -> Self {
        self.send_gate = Some(gate);
        self
    }
    fn frames(&mut self, frames: Vec<DecodedFrame>) -> Result<(), StorageError> {
        for mut frame in frames {
            if self.status >= 300 {
                if matches!(frame, DecodedFrame::ToolCallComplete { .. }) {
                    continue;
                }
                if let DecodedFrame::Native { observation } = &mut frame {
                    observation.continuation = None;
                }
            }
            match &frame {
                DecodedFrame::Identity { model, .. } => {
                    if model.is_some() {
                        self.provider_model = model.clone();
                    }
                }
                DecodedFrame::Usage { observation } => {
                    self.pending.push_back(InferenceFrame::UsageDimensions(
                        devworks_core::services::usage::dimensions::ObservedDimensions {
                            service_tier: observation.service_tier.clone(),
                            processing_region: observation.processing_region.clone(),
                        },
                    ));
                    self.usage_sequence += 1;
                    let n = |v: Option<u64>| {
                        v.map(i64::try_from)
                            .transpose()
                            .map_err(|_| invalid("provider usage exceeds accounting integer"))
                    };
                    let mut additional = Vec::new();
                    for write in &observation.cache_writes {
                        additional.push(MeasuredQuantity {
                            modality: "Text".into(),
                            charge_kind: "CacheWrite".into(),
                            unit: "Token".into(),
                            cache_write_ttl_ms: write.ttl_seconds.and_then(|n| {
                                i64::try_from(n).ok().and_then(|n| n.checked_mul(1000))
                            }),
                            quantity: n(Some(write.tokens))?.expect("provided"),
                        });
                    }
                    for (modality, kind, count) in [
                        ("Audio", "Input", observation.input_audio_tokens),
                        ("Audio", "Output", observation.output_audio_tokens),
                    ] {
                        if let Some(count) = count {
                            additional.push(MeasuredQuantity {
                                modality: modality.into(),
                                charge_kind: kind.into(),
                                unit: "Token".into(),
                                cache_write_ttl_ms: None,
                                quantity: n(Some(count))?.expect("provided"),
                            });
                        }
                    }
                    let usage = accounting::Observation {
                        key: format!("provider-{}", self.usage_sequence),
                        sequence: self.usage_sequence,
                        semantics: ObservationSemantics::Cumulative,
                        quantities: Quantities {
                            input_tokens: n(observation.input_tokens)?,
                            output_tokens: n(observation.output_tokens)?,
                            cached_read_tokens: n(observation.cached_read_tokens)?,
                            cache_write_tokens: n(observation.cache_write_tokens)?,
                            reasoning_tokens: n(observation.reasoning_tokens)?,
                            additional,
                        },
                        reported_cost: observation.cost.as_ref().and_then(|c| {
                            c.currency
                                .as_ref()
                                .map(|currency| accounting::ReportedCost {
                                    amount: c.amount.clone(),
                                    currency: currency.clone(),
                                })
                        }),
                        provider_model_id: self.provider_model.clone(),
                    };
                    // Raw evidence precedes its independently classified accounting observation.
                    self.pending.push_back(InferenceFrame::Usage(usage));
                    self.pending
                        .push_back(InferenceFrame::Output(frame.clone()));
                    continue;
                }
                DecodedFrame::ToolCallComplete { .. } => {
                    self.completed_items.push(frame.clone());
                    continue;
                }
                DecodedFrame::Native { observation } if observation.continuation.is_some() => {
                    self.completed_items.push(frame.clone());
                    continue;
                }
                DecodedFrame::Terminal { outcome, .. } => {
                    if self.status >= 300
                        || !matches!(
                            outcome,
                            DecodedOutcome::Completed | DecodedOutcome::ToolCallsReady
                        )
                    {
                        self.completed_items.clear();
                    }
                    self.terminal = Some(if self.status >= 300 {
                        AttemptEnd::Failed
                    } else {
                        outcome_kind(outcome)
                    });
                    continue;
                }
                _ => {}
            }
            self.pending.push_back(InferenceFrame::Output(frame));
        }
        Ok(())
    }
    fn interrupted(
        &mut self,
        outcome: DecodedOutcome,
        reason: &'static str,
    ) -> Result<(), StorageError> {
        self.ended = true;
        let frames = self.decoder.finish_interrupted(outcome, reason);
        self.frames(frames)
    }
}
fn invalid(message: &str) -> StorageError {
    StorageError::InvalidRecord(message.into())
}
fn outcome_kind(outcome: &DecodedOutcome) -> AttemptEnd {
    match outcome {
        DecodedOutcome::Completed => AttemptEnd::Completed,
        DecodedOutcome::ToolCallsReady => AttemptEnd::ToolCallsReady,
        DecodedOutcome::Incomplete => AttemptEnd::Incomplete,
        DecodedOutcome::Cancelled => AttemptEnd::Cancelled,
        DecodedOutcome::Failed => AttemptEnd::Failed,
        DecodedOutcome::OutcomeUnknown => AttemptEnd::UnknownTail,
    }
}
impl OwnedInferenceSource for NativeProviderSource {
    fn start(&mut self, attempt: &StoredInferenceAttempt) -> Result<(), StorageError> {
        self.start_cancellable(attempt, &Cancellation::new())
    }
    fn start_cancellable(
        &mut self,
        attempt: &StoredInferenceAttempt,
        cancellation: &Cancellation,
    ) -> Result<(), StorageError> {
        if attempt.attempt_id != self.prepared.identity.attempt_id.0
            || attempt.request_id != self.prepared.identity.request_id
            || attempt.connection_id != self.prepared.identity.binding.connection_id
            || attempt.catalog_model_id != self.prepared.identity.binding.model_id
            || attempt.endpoint_profile != format!("{:?}", self.prepared.profile)
            || attempt.state != "Dispatched"
        {
            return Err(StorageError::ScopeDenied);
        }
        let access = self
            .connections
            .access(&attempt.connection_id, None)
            .map_err(|_| {
                StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    "current provider access unavailable",
                )
            })?;
        if access.connection_id != attempt.connection_id
            || access.endpoint != self.endpoint
            || access.profile != attempt.endpoint_profile
            || access.credential_revision != self.credential_revision
        {
            return Err(StorageError::ScopeDenied);
        }
        let mut request = HttpRequest::post(
            format!(
                "{}{}",
                access.endpoint.trim_end_matches('/'),
                self.prepared.path
            ),
            "application/json",
            self.prepared.body.clone(),
        );
        for (name, value) in &self.prepared.headers {
            request = request.header(name, value);
        }
        match (&access.credential, &self.prepared.profile) {
            (
                AccessCredential::None,
                EndpointProfile::CompatibleChat | EndpointProfile::Embedding,
            ) => {}
            (AccessCredential::ApiKey(key), EndpointProfile::AnthropicMessages) => {
                request = request.secret_header("x-api-key", key.clone());
            }
            (
                AccessCredential::ApiKey(key),
                EndpointProfile::CompatibleChat
                | EndpointProfile::Embedding
                | EndpointProfile::OpenAiResponses
                | EndpointProfile::OpenRouterChat,
            ) => {
                request = request.secret_header(
                    "Authorization",
                    Secret::new(format!("Bearer {}", key.expose())),
                );
            }
            (
                AccessCredential::Bearer { token, account },
                EndpointProfile::CodexSubscriptionResponses
                | EndpointProfile::ClaudeSubscriptionMessages,
            ) => {
                request = request.secret_header(
                    "Authorization",
                    Secret::new(format!("Bearer {}", token.expose())),
                );
                if matches!(
                    self.prepared.profile,
                    EndpointProfile::CodexSubscriptionResponses
                ) {
                    let account = account
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .ok_or(StorageError::ScopeDenied)?;
                    request = request.secret_header("ChatGPT-Account-Id", Secret::new(account));
                    if let Some(key) = &self.prepared.session_key {
                        request = request.header("session_id", key);
                    }
                }
            }
            _ => return Err(StorageError::ScopeDenied),
        }
        request.timeout_ms = self.limits.deadline_ms;
        request.max_response_bytes = usize::try_from(self.limits.max_response_bytes)
            .map_err(|_| invalid("response bound"))?;
        if cancellation.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        if let Some(gate) = &self.send_gate {
            gate()?;
        }
        let stream = match self.http.open(&request, &self.limits, cancellation) {
            Ok(stream) => stream,
            Err(error) => {
                self.start_failure = if error.phase == SendPhase::NotSent {
                    AttemptEnd::Failed
                } else {
                    AttemptEnd::UnknownTail
                };
                return Err(StorageError::unavailable(
                    Unavailable::Io,
                    format!("provider transport {:?}: {:?}", error.phase, error.kind),
                ));
            }
        };
        self.status = stream.head().status;
        self.rejected = if self.status == 401 {
            match &access.credential {
                AccessCredential::Bearer { token, .. } => Some(token.clone()),
                _ => None,
            }
        } else {
            None
        };
        let headers = stream
            .head()
            .headers
            .iter()
            .filter(|(name, _)| {
                matches!(
                    name.to_ascii_lowercase().as_str(),
                    "retry-after" | "x-ratelimit-reset-requests" | "x-ratelimit-reset-tokens"
                ) || name.to_ascii_lowercase().starts_with("x-codex-")
                    || name
                        .to_ascii_lowercase()
                        .starts_with("anthropic-ratelimit-unified-")
            })
            .map(|(n, v)| (n.clone(), v.chars().take(512).collect::<String>()))
            .collect::<Vec<_>>();
        self.pending.push_back(InferenceFrame::ResponseHead(
            devworks_core::providers::ProviderResponseHead {
                status: self.status,
                headers,
                access: devworks_core::providers::ProviderAccessBasis {
                    connection_id: access.connection_id.clone(),
                    profile: self.prepared.profile.clone(),
                    endpoint_digest: devworks_core::fingerprint::sha256_hex(
                        access.endpoint.as_bytes(),
                    ),
                    credential_revision: access.credential_revision,
                    account_revision: self.prepared.identity.binding.account_revision.get(),
                    oauth: matches!(access.credential, AccessCredential::Bearer { .. }),
                },
            },
        ));
        let streaming = matches!(
            self.prepared.profile,
            EndpointProfile::CodexSubscriptionResponses
        ) || stream.head().header("content-type").is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/event-stream"))
        }) || self.prepared.response_mode == ResponseMode::Streaming;
        if streaming && self.status < 300 {
            self.sse = Some(
                devworks_core::providers::sse::SseDecoder::new(
                    devworks_core::providers::sse::SseLimits {
                        max_line_bytes: 8 * 1024 * 1024,
                        max_event_bytes: 8 * 1024 * 1024,
                        max_stream_bytes: self.limits.max_response_bytes as usize,
                        max_events: 65536,
                    },
                )
                .map_err(|_| invalid("event framing bounds"))?,
            );
        }
        self.stream = Some(stream);
        Ok(())
    }
    fn next(
        &mut self,
        cancellation: &Cancellation,
    ) -> Result<Option<InferenceFrame>, StorageError> {
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return Ok(Some(frame));
            }
            if let Some(terminal) = self.terminal.take() {
                self.close()?;
                self.pending
                    .extend(self.completed_items.drain(..).map(InferenceFrame::Output));
                self.pending.push_back(InferenceFrame::Terminal(terminal));
                continue;
            }
            if self.ended {
                return Ok(None);
            }
            let mut bytes = [0_u8; 16384];
            let result = self
                .stream
                .as_mut()
                .ok_or_else(|| invalid("provider source not started"))?
                .read(&mut bytes, cancellation);
            match result {
                Err(error) => {
                    let kind = match error.kind {
                        TransportFailure::Unreachable
                        | TransportFailure::Deadline
                        | TransportFailure::IdleTimeout => {
                            devworks_core::providers::decode::DecodeErrorKind::Unreachable
                        }
                        TransportFailure::Protocol | TransportFailure::InvalidRequest => {
                            devworks_core::providers::decode::DecodeErrorKind::Protocol
                        }
                        TransportFailure::TooLarge => {
                            devworks_core::providers::decode::DecodeErrorKind::LimitExceeded
                        }
                        TransportFailure::Cancelled => {
                            devworks_core::providers::decode::DecodeErrorKind::Provider
                        }
                    };
                    self.frames(vec![DecodedFrame::Error {
                        error: devworks_core::providers::decode::DecodeError {
                            kind,
                            code: Some(format!("transport_{:?}", error.kind)),
                            message: "provider transport interrupted".into(),
                        },
                    }])?;
                    self.interrupted(
                        DecodedOutcome::OutcomeUnknown,
                        "provider transport interrupted",
                    )?;
                }
                Ok(0) => {
                    self.ended = true;
                    let frames = if let Some(sse) = self.sse.as_mut() {
                        if sse.finish() == Ok(devworks_core::providers::sse::End::Complete) {
                            self.decoder.finish()
                        } else {
                            self.decoder.finish_interrupted(
                                DecodedOutcome::Incomplete,
                                "unterminated event stream",
                            )
                        }
                    } else {
                        self.decoder.consume_aggregate(&self.aggregate)
                    };
                    self.frames(frames)?;
                    if self.status >= 300 {
                        let kind = match self.status {
                            401 | 403 => {
                                devworks_core::providers::decode::DecodeErrorKind::Authentication
                            }
                            404 => {
                                devworks_core::providers::decode::DecodeErrorKind::ModelUnavailable
                            }
                            408 => devworks_core::providers::decode::DecodeErrorKind::Unreachable,
                            429 => devworks_core::providers::decode::DecodeErrorKind::Cooldown,
                            500 | 502 | 503 | 504 | 529 => {
                                devworks_core::providers::decode::DecodeErrorKind::Overloaded
                            }
                            _ => devworks_core::providers::decode::DecodeErrorKind::Provider,
                        };
                        // Keep the actual HTTP classification even if a proxy supplied a syntactically
                        // successful body. No calls from a non-success response become dispatchable.
                        self.pending
                            .push_front(InferenceFrame::Output(DecodedFrame::Error {
                                error: devworks_core::providers::decode::DecodeError {
                                    kind,
                                    code: Some(format!("http_{}", self.status)),
                                    message: "provider HTTP request refused".into(),
                                },
                            }));
                    }
                    if let Some(rejected) = self.rejected.take() {
                        self.stream
                            .as_mut()
                            .expect("started")
                            .close()
                            .map_err(|_| {
                                StorageError::unavailable(
                                    Unavailable::UnsupportedRuntime,
                                    "provider transport closure unconfirmed",
                                )
                            })?;
                        // Refresh lifecycle through its real owner. Never resend this inference
                        // attempt or concatenate another response after any exposed prefix.
                        let _ = self.connections.access(
                            &self.prepared.identity.binding.connection_id,
                            Some(&rejected),
                        );
                    }
                }
                Ok(n) => {
                    if let Some(sse) = self.sse.as_mut() {
                        let mut frames = Vec::new();
                        let decoder = &mut self.decoder;
                        let parsed = sse.push_events(&bytes[..n], |event| {
                            frames.extend(
                                decoder.consume_data_event(event.event.as_deref(), &event.data),
                            );
                        });
                        self.frames(frames)?;
                        if parsed.is_err() {
                            self.interrupted(DecodedOutcome::Incomplete, "malformed event stream")?;
                        } else if self.decoder.protocol_ended() {
                            // Drain visible text/evidence before waiting for physical close.
                            // Completed native/call items remain private until the terminal
                            // branch above confirms closure.
                            self.ended = true;
                            let frames = self.decoder.finish();
                            self.frames(frames)?;
                        }
                    } else {
                        if self.aggregate.len().saturating_add(n)
                            > self.limits.max_response_bytes as usize
                        {
                            self.interrupted(
                                DecodedOutcome::Incomplete,
                                "aggregate response exceeds bound",
                            )?;
                        } else {
                            self.aggregate.extend_from_slice(&bytes[..n]);
                        }
                    }
                }
            }
        }
    }
    fn start_failure_outcome(&self) -> AttemptEnd {
        self.start_failure
    }
    fn close(&mut self) -> Result<(), StorageError> {
        if let Some(stream) = &mut self.stream {
            stream.close().map_err(|_| {
                StorageError::unavailable(
                    Unavailable::UnsupportedRuntime,
                    "provider transport closure unconfirmed",
                )
            })?;
        }
        Ok(())
    }
}
