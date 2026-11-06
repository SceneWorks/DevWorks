//! Closed V1 material referenced by InferenceInput::Generation.context. No authority or billing.
use super::{ProviderError, ResponseMode};
use crate::protocol::provider::{ArtifactRef, NativeBinding};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderContext {
    pub version: u32,
    pub instructions: Vec<TextBlock>,
    pub messages: Vec<Message>,
    pub parameters: GenerationParameters,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TextBlock {
    pub text: String,
    #[serde(default)]
    pub cache: Option<CacheBreakpoint>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CacheBreakpoint {
    pub ttl: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Message {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default)]
        cache: Option<CacheBreakpoint>,
    },
    Image {
        artifact: ArtifactRef,
        #[serde(default)]
        detail: Option<ImageDetail>,
        #[serde(default)]
        cache: Option<CacheBreakpoint>,
    },
    ToolCall {
        call_id: String,
        wire_name: String,
        arguments: ArtifactRef,
    },
    ToolResult {
        call_id: String,
        content: Vec<ResultBlock>,
        is_error: bool,
        #[serde(default)]
        cache: Option<CacheBreakpoint>,
    },
    Native {
        binding: NativeBinding,
        artifact: ArtifactRef,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ResultBlock {
    Text {
        text: String,
    },
    Image {
        artifact: ArtifactRef,
        #[serde(default)]
        detail: Option<ImageDetail>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ImageDetail {
    Auto,
    Low,
    High,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum ToolChoice {
    Auto,
    Any,
    None,
    Tool { wire_name: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum Thinking {
    Disabled,
    Enabled { budget_tokens: u32 },
    Adaptive,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GenerationParameters {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub max_output_tokens: Option<u32>,
    pub stop: Option<Vec<String>>,
    pub tool_choice: Option<ToolChoice>,
    pub parallel_tool_calls: Option<bool>,
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_retention: Option<String>,
    pub thinking: Option<Thinking>,
    pub response_mode: Option<ResponseMode>,
    pub user_id: Option<String>,
}
impl ProviderContext {
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.version != 1 {
            return Err(ProviderError::InvalidInput("context_version"));
        }
        if self.messages.is_empty() || self.messages.len() > 4096 || self.instructions.len() > 256 {
            return Err(ProviderError::InvalidInput("context_messages"));
        }
        let p = &self.parameters;
        if p.temperature
            .is_some_and(|n| !n.is_finite() || !(0.0..=2.0).contains(&n))
            || p.top_p
                .is_some_and(|n| !n.is_finite() || !(0.0..=1.0).contains(&n))
            || p.top_k == Some(0)
            || p.max_output_tokens == Some(0)
            || p.stop
                .as_ref()
                .is_some_and(|s| s.len() > 128 || s.iter().any(|s| s.is_empty()))
        {
            return Err(ProviderError::InvalidInput("generation_parameters"));
        }
        if self
            .messages
            .iter()
            .any(|m| m.content.is_empty() || m.content.len() > 4096)
        {
            return Err(ProviderError::InvalidInput("context_blocks"));
        }
        Ok(())
    }
}
