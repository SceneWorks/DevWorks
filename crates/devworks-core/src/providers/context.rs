//! Closed V1 material referenced by InferenceInput::Generation.context. No authority or billing.
use super::{ProviderError, ResponseMode};
use crate::ports::storage::{StorageError, StoredEntry};
use crate::protocol::OperationId;
use crate::protocol::provider::{ArtifactRef, EndpointProfile, NativeBinding};
use crate::protocol::provider::{ToolOutcome, TranscriptBody, UserContent};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderContext {
    pub version: u32,
    pub instructions: Vec<TextBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instruction_stamps: Vec<super::instructions::InstructionStamp>,
    /// The effective skill set bound before repository guidance (its catalog block is the
    /// instruction block just before the repository suffix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_stamp: Option<super::instructions::SkillStamp>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        operation_id: Option<OperationId>,
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
        let provider_changed = previous.connection_id != next.connection_id
            || previous.account_revision != next.account_revision
            || previous.endpoint_profile != next.endpoint_profile
            || previous.model_id != next.model_id;
        if provider_changed {
            // Cache breakpoints describe the old upstream's prefix, not conversation meaning.
            // Remove them at the epoch boundary before publishing the destination context.
            for instruction in &mut self.instructions {
                instruction.cache = None;
            }
            for message in &mut self.messages {
                for block in &mut message.content {
                    match block {
                        ContentBlock::Text { cache, .. }
                        | ContentBlock::Image { cache, .. }
                        | ContentBlock::ToolResult { cache, .. } => *cache = None,
                        _ => {}
                    }
                }
            }
            self.parameters.tool_cache = None;
        }
        let p = &self.parameters;
        let incompatible = match next.endpoint_profile {
            EndpointProfile::CodexSubscriptionResponses => {
                p.temperature.is_some()
                    || p.top_p.is_some()
                    || p.top_k.is_some()
                    || p.stop.is_some()
                    || p.max_output_tokens.is_some()
                    || p.user_id.is_some()
            }
            EndpointProfile::OpenAiResponses => p.top_k.is_some() || p.stop.is_some(),
            EndpointProfile::AnthropicMessages | EndpointProfile::ClaudeSubscriptionMessages => {
                p.temperature.is_some_and(|value| value > 1.0)
                    || p.prompt_cache_key.is_some()
                    || p.prompt_cache_retention.is_some()
            }
            EndpointProfile::CompatibleChat | EndpointProfile::OpenRouterChat => p.top_k.is_some(),
            EndpointProfile::Embedding => true,
        } || (p.anthropic_betas.is_some()
            && !matches!(
                next.endpoint_profile,
                EndpointProfile::AnthropicMessages | EndpointProfile::ClaudeSubscriptionMessages
            ))
            || (p.thinking.is_some()
                && !matches!(
                    (&next.endpoint_profile, &p.thinking),
                    (
                        EndpointProfile::AnthropicMessages
                            | EndpointProfile::ClaudeSubscriptionMessages,
                        _
                    ) | (EndpointProfile::OpenRouterChat, Some(Thinking::Adaptive))
                ));
        if provider_changed && incompatible {
            return Err(ProviderError::Unsupported("epoch_destination_parameters"));
        }
        self.validate()?;
        Ok((self, omitted))
    }

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
        let session = entries.first().map(|entry| &entry.session_id);
        let epoch = entries.first().map(|entry| entry.context_epoch);
        for (next, entry) in (1..).zip(entries.iter()) {
            if entry.sequence != next
                || session != Some(&entry.session_id)
                || epoch != Some(entry.context_epoch)
            {
                return Err(StorageError::InvalidRecord(
                    "transcript history is not one contiguous epoch".into(),
                ));
            }
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
                TranscriptBody::ToolDispatchStarted { .. } => continue,
                TranscriptBody::ToolResponseRecorded { call, outcome } => {
                    let operation_id =
                        if let ToolOutcome::OperationAccepted { operation_id, .. }
                        | ToolOutcome::ApprovalRequired { operation_id, .. }
                        | ToolOutcome::OutcomeUnknown { operation_id, .. } = &outcome
                        {
                            Some(operation_id.clone())
                        } else {
                            None
                        };
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
                            operation_id,
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
                TranscriptBody::OperationUpdate {
                    operation_id,
                    artifact,
                    ..
                } => {
                    let result = messages.iter_mut().flat_map(|message: &mut Message| message.content.iter_mut())
                        .find(|block| matches!(block, ContentBlock::ToolResult { operation_id: Some(id), .. } if id == &operation_id))
                        .ok_or_else(|| StorageError::InvalidRecord("missing operation tool result".into()))?;
                    if let ContentBlock::ToolResult { content, .. } = result {
                        content.push(ResultBlock::Text {
                            text: read_text(&artifact)?,
                        });
                    }
                    continue;
                }
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
            instruction_stamps: Vec::new(),
            skill_stamp: None,
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
        if let Some(skills) = &self.skill_stamp {
            let repository: usize = self
                .instruction_stamps
                .iter()
                .map(|s| s.sources.len())
                .sum();
            let at = self.instructions.len().checked_sub(repository + 1);
            if !skills.validate()
                || at.is_none_or(|at| self.instructions[at].text != skills.text_block().text)
            {
                return Err(ProviderError::InvalidInput("skill_stamp"));
            }
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
