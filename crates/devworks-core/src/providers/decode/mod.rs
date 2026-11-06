//! Pure native response decoding. Transport framing, authority and artifact publication live
//! outside this module. Observations are evidence; only `finish` can release complete calls.
mod chat;
mod embedding;
mod messages;
mod responses;
mod usage;

use crate::protocol::provider::EndpointProfile;
use serde_json::Value;
use std::collections::BTreeMap;

pub use usage::{CacheWrite, DecodedUsage, InputSemantics, ReportedCost};

#[derive(Clone, Debug, PartialEq)]
pub enum DecodedFrame {
    Identity {
        model: Option<String>,
        generation_id: Option<String>,
    },
    TextDelta {
        block_index: u64,
        text: String,
    },
    ToolCallComplete {
        call: DecodedToolCall,
    },
    Native {
        observation: NativeObservation,
    },
    Usage {
        observation: DecodedUsage,
    },
    Embedding {
        index: u64,
        vector: Vec<f32>,
    },
    Error {
        error: DecodeError,
    },
    Terminal {
        outcome: DecodedOutcome,
        reason: Option<String>,
    },
    Heartbeat,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodedOutcome {
    Completed,
    ToolCallsReady,
    Incomplete,
    Cancelled,
    Failed,
    OutcomeUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeErrorKind {
    InvalidJson,
    InvalidContent,
    InvalidToolCall,
    InvalidEmbedding,
    UnsupportedReplay,
    Authentication,
    Permission,
    ModelUnavailable,
    ContextLimit,
    Cooldown,
    Overloaded,
    Refusal,
    Provider,
    LimitExceeded,
    Protocol,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError {
    pub kind: DecodeErrorKind,
    pub code: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeReplay {
    Supported,
    Unsupported,
}

/// Exact event bytes retain native numbers and extension fields even when the convenience
/// JSON projection cannot represent their decimal lexeme. No host ID is manufactured here.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeObservation {
    pub ordinal: Option<u64>,
    pub kind: String,
    pub value: Value,
    pub raw_json: String,
    pub replay: NativeReplay,
    /// Exact provider-native replay unit; stream envelopes are evidence only.
    pub continuation: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecodedToolCall {
    pub native_call_id: String,
    pub native_item_id: Option<String>,
    pub ordinal: u64,
    pub name: String,
    pub arguments_raw: String,
    pub arguments: Value,
    pub native_item: Value,
}

#[derive(Clone, Debug)]
pub struct DecodeLimits {
    pub event_bytes: usize,
    pub total_bytes: usize,
    pub tool_calls: usize,
    pub native_items: usize,
    pub embedding_dimensions: usize,
}
impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            event_bytes: 8 * 1024 * 1024,
            total_bytes: 32 * 1024 * 1024,
            tool_calls: 1024,
            native_items: 65536,
            embedding_dimensions: 65536,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Call {
    id: Option<String>,
    item_id: Option<String>,
    name: Option<String>,
    arguments: String,
    item: Value,
    complete: bool,
}

pub struct Decoder {
    profile: EndpointProfile,
    limits: DecodeLimits,
    bytes: usize,
    observations: usize,
    calls: BTreeMap<u64, Call>,
    blocks: BTreeMap<u64, Value>,
    seen_text: std::collections::BTreeSet<(u64, u64)>,
    stop: Option<(DecodedOutcome, Option<String>)>,
    ended: bool,
    failed: bool,
    unsupported: bool,
    finished: bool,
    model: Option<String>,
    generation_id: Option<String>,
    chat_reasoning: Value,
    continuations: BTreeMap<(u64, String), Value>,
}

impl Decoder {
    pub fn new(profile: EndpointProfile) -> Self {
        Self::with_limits(profile, DecodeLimits::default())
    }
    pub fn with_limits(profile: EndpointProfile, limits: DecodeLimits) -> Self {
        Self {
            profile,
            limits,
            bytes: 0,
            observations: 0,
            calls: BTreeMap::new(),
            blocks: BTreeMap::new(),
            seen_text: Default::default(),
            stop: None,
            ended: false,
            failed: false,
            unsupported: false,
            finished: false,
            model: None,
            generation_id: None,
            chat_reasoning: serde_json::json!({}),
            continuations: BTreeMap::new(),
        }
    }
    /// Consume one already-framed SSE data event. The adapter owns byte fragmentation, UTF-8,
    /// comment/multiline framing and event-ID handling; event JSON remains native here.
    pub fn consume_data_event(&mut self, event: Option<&str>, data: &str) -> Vec<DecodedFrame> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        if !self.accept(data, &mut out) {
            return out;
        }
        if data.trim() == "[DONE]" {
            if matches!(
                self.profile,
                EndpointProfile::CompatibleChat | EndpointProfile::OpenRouterChat
            ) {
                self.ended = true;
            } else {
                self.error(DecodeErrorKind::Protocol, "unexpected_done", &mut out);
            }
            return out;
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => {
                self.error(DecodeErrorKind::InvalidJson, "invalid_event_json", &mut out);
                return out;
            }
        };
        self.observe_identity(&value, &mut out);
        match self.profile {
            EndpointProfile::OpenAiResponses | EndpointProfile::CodexSubscriptionResponses => {
                self.responses_event(event, &value, data, &mut out)
            }
            EndpointProfile::AnthropicMessages | EndpointProfile::ClaudeSubscriptionMessages => {
                self.messages_event(event, &value, data, &mut out)
            }
            EndpointProfile::OpenRouterChat | EndpointProfile::CompatibleChat => {
                self.chat_event(&value, data, false, &mut out)
            }
            EndpointProfile::Embedding => self.embedding(&value, data, &mut out),
        }
        out
    }

    /// Aggregate JSON returns observations before the terminal, including independent usage
    /// when an embedding or content block is invalid. An aggregate is a complete transport body.
    pub fn consume_aggregate(&mut self, bytes: &[u8]) -> Vec<DecodedFrame> {
        let mut out = Vec::new();
        let Ok(raw) = std::str::from_utf8(bytes) else {
            self.error(DecodeErrorKind::InvalidJson, "invalid_utf8", &mut out);
            out.extend(self.finish());
            return out;
        };
        if !self.accept(raw, &mut out) {
            out.extend(self.finish());
            return out;
        }
        let value: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => {
                self.error(DecodeErrorKind::InvalidJson, "invalid_body_json", &mut out);
                out.extend(self.finish());
                return out;
            }
        };
        self.observe_identity(&value, &mut out);
        match self.profile {
            EndpointProfile::OpenAiResponses | EndpointProfile::CodexSubscriptionResponses => {
                self.responses_body(&value, raw, &mut out)
            }
            EndpointProfile::AnthropicMessages | EndpointProfile::ClaudeSubscriptionMessages => {
                self.messages_body(&value, raw, &mut out)
            }
            EndpointProfile::CompatibleChat | EndpointProfile::OpenRouterChat => {
                self.chat_event(&value, raw, true, &mut out);
                self.ended = true;
            }
            EndpointProfile::Embedding => self.embedding(&value, raw, &mut out),
        }
        out.extend(self.finish());
        out
    }

    /// EOF is never evidence of successful completion. This is the sole tool-release point;
    /// incomplete/error/unsupported streams retain their observations but release no calls.
    pub fn finish(&mut self) -> Vec<DecodedFrame> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut out = Vec::new();
        if self
            .chat_reasoning
            .as_object()
            .is_some_and(|o| !o.is_empty())
        {
            let value = self.chat_reasoning.clone();
            self.continuation(Some(0), "chat_reasoning", &value, &mut out);
        }
        for ((ordinal, kind), value) in &self.continuations {
            out.push(DecodedFrame::Native {
                observation: NativeObservation {
                    ordinal: Some(*ordinal),
                    kind: kind.clone(),
                    raw_json: value.to_string(),
                    value: value.clone(),
                    replay: NativeReplay::Supported,
                    continuation: Some(value.clone()),
                },
            });
        }
        let (mut outcome, reason) = self.stop.clone().unwrap_or((
            DecodedOutcome::OutcomeUnknown,
            Some("missing_native_terminal".into()),
        ));
        if !self.ended {
            outcome = DecodedOutcome::OutcomeUnknown;
        }
        if self.failed || self.unsupported {
            outcome = DecodedOutcome::Failed;
        }
        if outcome == DecodedOutcome::Completed {
            let mut ready = Vec::new();
            for (ordinal, call) in &self.calls {
                let parsed = serde_json::from_str::<Value>(&call.arguments)
                    .ok()
                    .filter(Value::is_object);
                if !call.complete
                    || call
                        .id
                        .as_ref()
                        .is_none_or(|s| s.is_empty() || s.len() > 1024)
                    || call
                        .name
                        .as_ref()
                        .is_none_or(|s| s.is_empty() || s.len() > 1024)
                    || parsed.is_none()
                {
                    outcome = DecodedOutcome::Failed;
                    out.push(DecodedFrame::Error {
                        error: DecodeError {
                            kind: DecodeErrorKind::InvalidToolCall,
                            code: None,
                            message:
                                "missing or invalid native tool-call identity/arguments/completion"
                                    .into(),
                        },
                    });
                    break;
                }
                ready.push(DecodedToolCall {
                    native_call_id: call.id.clone().unwrap(),
                    native_item_id: call.item_id.clone(),
                    ordinal: *ordinal,
                    name: call.name.clone().unwrap(),
                    arguments_raw: call.arguments.clone(),
                    arguments: parsed.unwrap(),
                    native_item: call.item.clone(),
                });
            }
            if outcome == DecodedOutcome::Completed && !ready.is_empty() {
                for call in ready {
                    out.push(DecodedFrame::ToolCallComplete { call });
                }
                outcome = DecodedOutcome::ToolCallsReady;
            }
        }
        out.push(DecodedFrame::Terminal { outcome, reason });
        out
    }

    fn accept(&mut self, data: &str, out: &mut Vec<DecodedFrame>) -> bool {
        self.bytes = self.bytes.saturating_add(data.len());
        if data.len() > self.limits.event_bytes || self.bytes > self.limits.total_bytes {
            self.error(DecodeErrorKind::LimitExceeded, "response_bytes", out);
            false
        } else {
            true
        }
    }
    fn error(&mut self, kind: DecodeErrorKind, message: &str, out: &mut Vec<DecodedFrame>) {
        self.failed = true;
        out.push(DecodedFrame::Error {
            error: DecodeError {
                kind,
                code: None,
                message: message.into(),
            },
        });
    }
    fn native(
        &mut self,
        ordinal: Option<u64>,
        kind: &str,
        value: &Value,
        raw: &str,
        supported: bool,
        out: &mut Vec<DecodedFrame>,
    ) {
        self.observations += 1;
        if self.observations > self.limits.native_items {
            self.error(DecodeErrorKind::LimitExceeded, "native_items", out);
            return;
        }
        out.push(DecodedFrame::Native {
            observation: NativeObservation {
                ordinal,
                kind: kind.into(),
                value: value.clone(),
                raw_json: raw.into(),
                continuation: None,
                replay: if supported {
                    NativeReplay::Supported
                } else {
                    NativeReplay::Unsupported
                },
            },
        });
        if !supported {
            self.unsupported = true;
            self.error(DecodeErrorKind::UnsupportedReplay, kind, out);
        }
    }
    fn observe_identity(&mut self, value: &Value, out: &mut Vec<DecodedFrame>) {
        let body = value
            .get("response")
            .or_else(|| value.get("message"))
            .unwrap_or(value);
        let model = string(body, "model");
        let id = string(body, "id");
        if model.is_some() && model != self.model || id.is_some() && id != self.generation_id {
            if model.is_some() {
                self.model = model;
            }
            if id.is_some() {
                self.generation_id = id;
            }
            out.push(DecodedFrame::Identity {
                model: self.model.clone(),
                generation_id: self.generation_id.clone(),
            });
        }
    }
    fn continuation(
        &mut self,
        ordinal: Option<u64>,
        kind: &str,
        value: &Value,
        _out: &mut Vec<DecodedFrame>,
    ) {
        if let Some(ordinal) = ordinal {
            self.continuations
                .insert((ordinal, kind.into()), value.clone());
        }
    }
    fn call(&mut self, index: u64, out: &mut Vec<DecodedFrame>) -> Option<&mut Call> {
        if !self.calls.contains_key(&index) && self.calls.len() >= self.limits.tool_calls {
            self.error(DecodeErrorKind::LimitExceeded, "tool_calls", out);
            return None;
        }
        Some(self.calls.entry(index).or_default())
    }
    fn provider_error(&mut self, error: &Value, out: &mut Vec<DecodedFrame>) {
        let code = string(error, "code")
            .or_else(|| string(error, "type"))
            .or_else(|| error.get("code").map(Value::to_string));
        let message = string(error, "message").unwrap_or_else(|| "provider error".into());
        let facts = format!("{} {}", code.as_deref().unwrap_or(""), message).to_ascii_lowercase();
        let kind = if facts.contains("authentication")
            || facts.contains("invalid_api_key")
            || code.as_deref() == Some("401")
        {
            DecodeErrorKind::Authentication
        } else if facts.contains("permission") || code.as_deref() == Some("403") {
            DecodeErrorKind::Permission
        } else if facts.contains("context_length")
            || facts.contains("context window")
            || facts.contains("too many tokens")
        {
            DecodeErrorKind::ContextLimit
        } else if facts.contains("rate_limit") || code.as_deref() == Some("429") {
            DecodeErrorKind::Cooldown
        } else if facts.contains("overload")
            || code.as_deref() == Some("529")
            || code.as_deref() == Some("503")
        {
            DecodeErrorKind::Overloaded
        } else if facts.contains("model_not_found") || facts.contains("invalid model") {
            DecodeErrorKind::ModelUnavailable
        } else {
            DecodeErrorKind::Provider
        };
        self.failed = true;
        out.push(DecodedFrame::Error {
            error: DecodeError {
                kind,
                code,
                message,
            },
        });
        self.stop = Some((
            DecodedOutcome::Failed,
            string(error, "code").or_else(|| string(error, "type")),
        ));
    }
}
fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn index(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

#[cfg(test)]
mod tests;
