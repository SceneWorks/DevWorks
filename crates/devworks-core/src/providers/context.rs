//! Closed V1 material referenced by InferenceInput::Generation.context. No authority or billing.
use super::{ProviderError, ResponseMode};
use crate::ports::storage::{StorageError, StoredEntry};
use crate::protocol::provider::{ArtifactRef, NativeBinding};
use crate::protocol::provider::{ToolOutcome, TranscriptBody, UserContent};
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
    /// Explicit breakpoints on current frozen tool aliases; never reorder tool definitions.
    pub tool_cache: Option<std::collections::BTreeMap<String, CacheBreakpoint>>,
    pub thinking: Option<Thinking>,
    /// Required protocol beta tokens; only exact host-qualified tokens may be sent.
    pub anthropic_betas: Option<Vec<String>>,
    pub response_mode: Option<ResponseMode>,
    pub user_id: Option<String>,
}
impl ProviderContext {
    /// Build provider history from immutable transcript entries in sequence order. Artifact text
    /// and the frozen wire alias are resolved by the host under the current session authority.
    /// Unsupported epoch transitions are refused rather than silently dropping old context.
    pub fn from_transcript(
        entries: &[StoredEntry],
        instructions: Vec<TextBlock>,
        parameters: GenerationParameters,
        mut read_text: impl FnMut(&ArtifactRef) -> Result<String, StorageError>,
        mut wire_alias: impl FnMut(&crate::protocol::provider::QualifiedId) -> Option<String>,
    ) -> Result<Self, StorageError> {
        let mut messages = Vec::new();
        let mut next = 1;
        for entry in entries {
            if entry.sequence != next {
                return Err(StorageError::InvalidRecord(
                    "transcript history has a gap".into(),
                ));
            }
            next += 1;
            let body: TranscriptBody = serde_json::from_value(entry.body.clone())
                .map_err(|_| StorageError::InvalidRecord("invalid transcript body".into()))?;
            let (role, content) = match body {
                TranscriptBody::UserMessage { content } => {
                    let mut blocks = Vec::new();
                    for item in content {
                        blocks.push(match item {
                            UserContent::Text { text } => ContentBlock::Text { text, cache: None },
                            UserContent::Attachment { artifact } => {
                                if !artifact.media_type.starts_with("image/") {
                                    return Err(StorageError::InvalidRecord(
                                        "unsupported user attachment".into(),
                                    ));
                                }
                                ContentBlock::Image {
                                    artifact,
                                    detail: None,
                                    cache: None,
                                }
                            }
                        });
                    }
                    (MessageRole::User, blocks)
                }
                TranscriptBody::AssistantAttempt { visible_text, .. } => (
                    MessageRole::Assistant,
                    vec![ContentBlock::Text {
                        text: visible_text,
                        cache: None,
                    }],
                ),
                TranscriptBody::HostNotice { artifact, .. } => (
                    MessageRole::User,
                    vec![ContentBlock::Text {
                        text: read_text(&artifact)?,
                        cache: None,
                    }],
                ),
                TranscriptBody::ToolCallRecorded { tool_call } => {
                    let wire_name = wire_alias(&tool_call.capability_id).ok_or_else(|| {
                        StorageError::InvalidRecord("missing frozen tool alias".into())
                    })?;
                    (
                        MessageRole::Assistant,
                        vec![ContentBlock::ToolCall {
                            call_id: tool_call.call.native_call_id,
                            wire_name,
                            arguments: tool_call.arguments,
                        }],
                    )
                }
                TranscriptBody::ToolResponseRecorded { call, outcome } => {
                    let (content, is_error) = match outcome {
                        ToolOutcome::Completed { artifacts } => {
                            let mut blocks = Vec::new();
                            for artifact in artifacts {
                                blocks.push(if artifact.media_type.starts_with("image/") {
                                    ResultBlock::Image {
                                        artifact,
                                        detail: None,
                                    }
                                } else if artifact.media_type.starts_with("text/")
                                    || artifact.media_type == "application/json"
                                {
                                    ResultBlock::Text {
                                        text: read_text(&artifact)?,
                                    }
                                } else {
                                    return Err(StorageError::InvalidRecord(
                                        "unsupported tool result artifact".into(),
                                    ));
                                });
                            }
                            (blocks, false)
                        }
                        other => (
                            vec![ResultBlock::Text {
                                text: serde_json::to_string(&other).map_err(|_| {
                                    StorageError::InvalidRecord("invalid tool outcome".into())
                                })?,
                            }],
                            true,
                        ),
                    };
                    (
                        MessageRole::User,
                        vec![ContentBlock::ToolResult {
                            call_id: call.native_call_id,
                            content,
                            is_error,
                            cache: None,
                        }],
                    )
                }
                TranscriptBody::NativeItem { binding, artifact } => (
                    MessageRole::Assistant,
                    vec![ContentBlock::Native { binding, artifact }],
                ),
                TranscriptBody::OperationUpdate { artifact, .. } => (
                    MessageRole::User,
                    vec![ContentBlock::Text {
                        text: read_text(&artifact)?,
                        cache: None,
                    }],
                ),
                TranscriptBody::ContextEpochChanged { .. } => {
                    return Err(StorageError::InvalidRecord(
                        "context epoch snapshot required".into(),
                    ));
                }
            };
            messages.push(Message { role, content });
        }
        let context = Self {
            version: 1,
            instructions,
            messages,
            parameters,
        };
        context.validate().map_err(|error| {
            StorageError::InvalidRecord(format!("invalid provider context: {error:?}"))
        })?;
        Ok(context)
    }

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
