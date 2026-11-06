//! Host-declared support for one exact model/profile/access binding; unknown never satisfies a need.
use crate::protocol::provider::NativeBinding;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    Supported,
    Unsupported,
    Unknown,
}
impl Support {
    pub fn supported(self) -> bool {
        self == Self::Supported
    }
}
#[derive(Clone, Debug)]
pub struct Capabilities {
    pub binding: NativeBinding,
    pub images: Support,
    pub tools: Support,
    pub parallel_tools: Support,
    pub structured_output: Support,
    pub embeddings: Support,
    pub embedding_dimensions: EmbeddingDimensions,
    pub sampling: Support,
    pub top_k: Support,
    pub stop: Support,
    pub thinking: Support,
    pub prompt_cache_key: Support,
    pub prompt_cache_retention: Support,
    pub cache_breakpoints: Support,
    pub cache_tool_breakpoints: Support,
    pub cache_minimum_input_tokens: Option<u32>,
    pub cache_maximum_breakpoints: Option<usize>,
    pub efforts: Vec<String>,
    /// Exact configured mapping; compatible syntax alone never implies support.
    pub effort_encodings: std::collections::BTreeMap<String, EffortEncoding>,
    pub cache_retentions: Vec<String>,
    pub cache_breakpoint_ttls: Vec<String>,
    pub anthropic_betas: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum EffortEncoding {
    ReasoningEffort { value: String },
    ChatTemplateKwargs { value: serde_json::Value },
    MessagesOutputConfig { value: String },
    ResponsesReasoning { value: String },
}
impl Capabilities {
    /// No support is inferred from a brand name or OpenAI-compatible syntax.
    pub fn unknown(binding: NativeBinding) -> Self {
        Self {
            binding,
            images: Support::Unknown,
            tools: Support::Unknown,
            parallel_tools: Support::Unknown,
            structured_output: Support::Unknown,
            embeddings: Support::Unknown,
            embedding_dimensions: EmbeddingDimensions::Unknown,
            sampling: Support::Unknown,
            top_k: Support::Unknown,
            stop: Support::Unknown,
            thinking: Support::Unknown,
            prompt_cache_key: Support::Unknown,
            prompt_cache_retention: Support::Unknown,
            cache_breakpoints: Support::Unknown,
            cache_tool_breakpoints: Support::Unknown,
            cache_minimum_input_tokens: None,
            cache_maximum_breakpoints: None,
            efforts: Vec::new(),
            effort_encodings: Default::default(),
            cache_retentions: Vec::new(),
            cache_breakpoint_ttls: Vec::new(),
            anthropic_betas: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmbeddingDimensions {
    Unknown,
    /// The model has this native output size; sending a dimensions parameter is unsupported.
    Fixed(u32),
    /// The configured model/profile explicitly supports the request dimensions parameter.
    Parameter,
}
