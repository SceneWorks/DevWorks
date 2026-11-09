//! Closed V1 material referenced by InferenceInput::Generation.context. No authority or billing.
use super::{ProviderError, ResponseMode};
use crate::protocol::provider::{ArtifactRef, NativeBinding};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderContext {
    pub version: u32,
    pub instructions: Vec<TextBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instruction_stamps: Vec<super::instructions::InstructionStamp>,
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
    /// Explicit breakpoints on current frozen tool aliases; never reorder tool definitions.
    pub tool_cache: Option<std::collections::BTreeMap<String, CacheBreakpoint>>,
    pub thinking: Option<Thinking>,
    /// Required protocol beta tokens; only exact host-qualified tokens may be sent.
    pub anthropic_betas: Option<Vec<String>>,
    pub response_mode: Option<ResponseMode>,
    pub user_id: Option<String>,
}
impl ProviderContext {
    /// Construct the immutable snapshot for an explicit context-epoch boundary. Generic text,
    /// images and completed tool exchanges remain in order; provider-owned opaque state cannot
    /// cross the boundary. The caller records the omitted references with ContextEpochChanged.
    pub fn converted_for_new_epoch(
        mut self,
        previous: &NativeBinding,
        next: &NativeBinding,
    ) -> Result<(Self, Vec<ArtifactRef>), ProviderError> {
        self.validate()?;
        if previous.context_epoch.get().checked_add(1) != Some(next.context_epoch.get()) {
            return Err(ProviderError::InvalidInput("context_epoch_boundary"));
        }
        let mut omitted = Vec::new();
        let mut messages = Vec::with_capacity(self.messages.len());
        for mut message in self.messages {
            let mut content = Vec::with_capacity(message.content.len());
            for block in message.content {
                match block {
                    ContentBlock::Native { binding, artifact } if &binding == previous => {
                        omitted.push(artifact);
                    }
                    ContentBlock::Native { .. } => return Err(ProviderError::ForeignNative),
                    _ => content.push(block),
                }
            }
            if !content.is_empty() {
                message.content = content;
                messages.push(message);
            }
        }
        self.messages = messages;
        self.validate()?;
        Ok((self, omitted))
    }

    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.version != 1 {
            return Err(ProviderError::InvalidInput("context_version"));
        }
        if self.messages.is_empty() || self.messages.len() > 4096 || self.instructions.len() > 256 {
            return Err(ProviderError::InvalidInput("context_messages"));
        }
        if self.instruction_stamps.len() > 64
            || self
                .instruction_stamps
                .iter()
                .any(|stamp| !stamp.validate())
            || !super::instructions::stamps_match_context(
                &self.instructions,
                &self.instruction_stamps,
            )
        {
            return Err(ProviderError::InvalidInput("instruction_stamps"));
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
        let mut outstanding = std::collections::BTreeSet::<String>::new();
        for message in &self.messages {
            for block in &message.content {
                match block {
                    ContentBlock::ToolCall {
                        call_id, wire_name, ..
                    } => {
                        if !matches!(message.role, MessageRole::Assistant)
                            || call_id.is_empty()
                            || call_id.len() > 1024
                            || wire_name.is_empty()
                            || wire_name.len() > 128
                            || !outstanding.insert(call_id.clone())
                        {
                            return Err(ProviderError::InvalidInput("tool_call_order"));
                        }
                    }
                    ContentBlock::ToolResult { call_id, .. }
                        if matches!(message.role, MessageRole::User)
                            && outstanding.remove(call_id) => {}
                    ContentBlock::ToolResult { .. } => {
                        return Err(ProviderError::InvalidInput("tool_result_order"));
                    }
                    _ => {}
                }
            }
        }
        if !outstanding.is_empty() {
            return Err(ProviderError::InvalidInput("unanswered_tool_call"));
        }
        Ok(())
    }
}
