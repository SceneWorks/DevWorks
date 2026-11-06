//! One pure effort interpretation for admission, accounting, and native encoding.
//! The frozen budget thresholds are retained; unsupported levels never round to a neighbor.
use super::{
    ProviderError,
    capabilities::{Capabilities, EffortEncoding},
    context::{GenerationParameters, Thinking},
};
use crate::protocol::provider::{EndpointProfile, InferenceOptions};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}
impl NativeEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
    fn parse(value: &str) -> Result<Self, ProviderError> {
        match value.to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            _ => Err(ProviderError::Unsupported("effort")),
        }
    }
}
#[derive(Clone, Debug)]
pub struct NormalizedEffort {
    pub requested: Option<NativeEffort>,
    pub effective: Option<NativeEffort>,
    /// None also covers native Messages thinking, which must retain its exact budget/type.
    pub encoding: Option<EffortEncoding>,
}
/// Explicit effort takes priority over thinking, as in the retained source. Adaptive/absent
/// thinking leaves the target's default unspecified; disabled is the distinct native `none`.
pub fn requested_effort(
    p: &GenerationParameters,
    options: &InferenceOptions,
) -> Result<Option<NativeEffort>, ProviderError> {
    if let Some(effort) = &options.effort {
        return NativeEffort::parse(effort.value.as_str()).map(Some);
    }
    Ok(match p.thinking {
        None | Some(Thinking::Adaptive) => None,
        Some(Thinking::Disabled) => Some(NativeEffort::None),
        Some(Thinking::Enabled { budget_tokens }) => Some(match budget_tokens {
            0..4096 => NativeEffort::Low,
            4096..16384 => NativeEffort::Medium,
            16384..32768 => NativeEffort::High,
            32768..65536 => NativeEffort::XHigh,
            _ => NativeEffort::Max,
        }),
    })
}
pub fn normalize_effort(
    p: &GenerationParameters,
    options: &InferenceOptions,
    caps: &Capabilities,
) -> Result<NormalizedEffort, ProviderError> {
    let requested = requested_effort(p, options)?;
    if p.thinking.is_some() && !caps.thinking.supported() {
        return Err(ProviderError::Unsupported("thinking"));
    }
    let profile = &caps.binding.endpoint_profile;
    if profile == &EndpointProfile::Embedding && (p.thinking.is_some() || requested.is_some()) {
        return Err(ProviderError::Unsupported("thinking"));
    }
    let messages = matches!(
        profile,
        EndpointProfile::AnthropicMessages | EndpointProfile::ClaudeSubscriptionMessages
    );
    let encoding = if messages && options.effort.is_none() {
        // Native Messages budget/adaptive/disabled is preserved, never replaced by a level.
        None
    } else if let Some(level) = requested {
        let key = caps
            .efforts
            .iter()
            .find(|v| v.eq_ignore_ascii_case(level.as_str()))
            .ok_or(ProviderError::Unsupported("effort"))?;
        let encoding = caps
            .effort_encodings
            .get(key)
            .ok_or(ProviderError::Unsupported("effort"))?;
        let compatible = matches!(
            (profile, encoding),
            (
                EndpointProfile::CompatibleChat | EndpointProfile::OpenRouterChat,
                EffortEncoding::ReasoningEffort { .. }
            ) | (
                EndpointProfile::CompatibleChat,
                EffortEncoding::ChatTemplateKwargs { .. }
            ) | (
                EndpointProfile::AnthropicMessages | EndpointProfile::ClaudeSubscriptionMessages,
                EffortEncoding::MessagesOutputConfig { .. }
            ) | (
                EndpointProfile::OpenAiResponses | EndpointProfile::CodexSubscriptionResponses,
                EffortEncoding::ResponsesReasoning { .. }
            )
        );
        if !compatible {
            return Err(ProviderError::Unsupported("effort_encoding"));
        }
        Some(encoding.clone())
    } else {
        None
    };
    Ok(NormalizedEffort {
        requested,
        effective: requested,
        encoding,
    })
}
