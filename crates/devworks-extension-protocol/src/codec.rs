//! Package-emitted provider codec events adopted from the recorded candidates
//! (tools/contracts-eval/src/provider.rs). Untrusted observations: the host adds price
//! classification, route binding and attribution before anything becomes durable usage.
use crate::scalars::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Codec observations are untrusted; the host adds price classification, route binding and attribution.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RawUsageObservation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_read_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<Counter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_cost: Option<ReportedCost>,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReportedCost {
    #[schemars(regex(pattern = "^[0-9]+(\\.[0-9]+)?$"))]
    pub amount: String,
    pub currency: Id,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum CodecBody {
    TextDelta {
        block_index: Counter,
        #[schemars(length(max = 65536))]
        text: String,
    },
    ToolCallComplete {
        tool_call: CompleteToolCall,
    },
    NativeContinuation {
        binding: NativeBinding,
        artifact: ArtifactRef,
    },
    Usage {
        observation: RawUsageObservation,
    },
    Terminal {
        outcome: TurnOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        native_stop_reason: Option<String>,
    },
    Heartbeat,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderCodecEvent {
    pub request_id: Id,
    pub attempt_id: AttemptId,
    pub sequence: Revision,
    pub event: CodecBody,
}
impl ProviderCodecEvent {
    pub fn validate(&self) -> Result<(), &'static str> {
        match &self.event {
            CodecBody::TextDelta { text, .. } if text.len() > 65536 => {
                return Err("codec text too large");
            }
            CodecBody::ToolCallComplete { tool_call } => {
                tool_call.call.validate()?;
                if tool_call.call.attempt_id != self.attempt_id {
                    return Err("codec foreign call");
                }
            }
            CodecBody::Usage { observation } => {
                if let Some(cost) = &observation.reported_cost {
                    let parts: Vec<_> = cost.amount.split('.').collect();
                    if parts.len() > 2
                        || parts
                            .iter()
                            .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()))
                    {
                        return Err("invalid reported decimal");
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}
