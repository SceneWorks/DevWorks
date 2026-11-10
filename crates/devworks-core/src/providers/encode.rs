//! Pure profile encoders. No socket, credentials, mutable catalog or implicit content repair.
use super::{
    capabilities::{Capabilities, EffortEncoding, EmbeddingDimensions},
    context::*,
    materialize::{self, Materials},
    *,
};
use crate::protocol::provider::{ArtifactRef, InferenceInput, InferenceRequest};
use crate::protocol::records::OpenRouterConstraints;
use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct PreparationLimits {
    pub context_bytes: u64,
    pub artifact_bytes: u64,
    pub request_bytes: usize,
}
struct Encoder<'a> {
    materials: &'a dyn Materials,
    identity: &'a RequestIdentity,
    limits: &'a PreparationLimits,
    expanded_artifact_bytes: std::cell::Cell<usize>,
}

pub fn prepare(
    identity: RequestIdentity,
    request: &InferenceRequest,
    capabilities: &Capabilities,
    materials: &dyn Materials,
    constraints: Option<&OpenRouterConstraints>,
    limits: &PreparationLimits,
) -> Result<PreparedRequest, ProviderError> {
    request.validate().map_err(ProviderError::InvalidInput)?;
    let tracked = materialize::TrackedMaterials {
        source: materials,
        references: Default::default(),
    };
    let materials: &dyn Materials = &tracked;
    if identity.request_id != request.request_id
        || capabilities.binding != identity.binding
        || identity.upstream_model.is_empty()
    {
        return Err(ProviderError::InvalidInput("attempt_identity"));
    }
    let profile = identity.binding.endpoint_profile.clone();
    let session_key = match &request.input {
        InferenceInput::Generation { session_id, .. } => Some(crate::fingerprint::sha256_hex(
            session_id.as_str().as_bytes(),
        )),
        InferenceInput::Embedding { .. } => None,
    };
    let encoder = Encoder {
        materials,
        identity: &identity,
        limits,
        expanded_artifact_bytes: std::cell::Cell::new(0),
    };
    let mut required_betas = Vec::new();
    let (body, tools, response_mode, embedding_dimensions) = match &request.input {
        InferenceInput::Embedding {
            dimensions, inputs, ..
        } => {
            if profile != EndpointProfile::Embedding || !capabilities.embeddings.supported() {
                return Err(ProviderError::Unsupported("embeddings"));
            }
            let text = inputs
                .iter()
                .map(|r| encoder.text(r, "embedding_utf8"))
                .collect::<Result<Vec<_>, _>>()?;
            let mut body =
                json!({"model":identity.upstream_model,"input":text,"encoding_format":"float"});
            match capabilities.embedding_dimensions {
                EmbeddingDimensions::Parameter => body["dimensions"] = json!(dimensions),
                EmbeddingDimensions::Fixed(native) if native == *dimensions => {}
                EmbeddingDimensions::Fixed(_) => {
                    return Err(ProviderError::Unsupported("embedding_dimensions"));
                }
                EmbeddingDimensions::Unknown => {
                    return Err(ProviderError::Unsupported("embedding_dimensions_unknown"));
                }
            }
            (body, Vec::new(), ResponseMode::Aggregate, Some(*dimensions))
        }
        InferenceInput::Generation {
            context,
            context_epoch,
            tools,
            response_headroom_tokens,
            requires_images,
            requires_parallel_tools,
            ..
        } => {
            if profile == EndpointProfile::Embedding
                || context_epoch != &identity.binding.context_epoch
            {
                return Err(ProviderError::InvalidInput("generation_binding"));
            }
            let bytes = materials.artifact(context, limits.context_bytes)?;
            let context: ProviderContext = serde_json::from_slice(&bytes)
                .map_err(|_| ProviderError::InvalidInput("provider_context"))?;
            context.validate()?;
            required_betas = context
                .parameters
                .anthropic_betas
                .clone()
                .unwrap_or_default();
            check(
                &context,
                request,
                capabilities,
                *requires_images,
                *requires_parallel_tools,
            )?;
            let tools = materialize::tools(materials, tools, |tool| {
                encoder.charge_json(&(&tool.schema, &tool.description, &tool.wire_name))
            })?;
            let body = encoder.generation(
                &context,
                request,
                &tools,
                response_headroom_tokens.get(),
                constraints,
                capabilities,
            )?;
            let response_mode = context
                .parameters
                .response_mode
                .unwrap_or(ResponseMode::Streaming);
            (body, tools, response_mode, None)
        }
    };
    let bytes = bounded_json(&body, limits.request_bytes)?;
    let path = match profile {
        EndpointProfile::AnthropicMessages | EndpointProfile::ClaudeSubscriptionMessages => {
            "/v1/messages"
        }
        EndpointProfile::OpenAiResponses | EndpointProfile::CodexSubscriptionResponses => {
            "/responses"
        }
        EndpointProfile::OpenRouterChat | EndpointProfile::CompatibleChat => "/chat/completions",
        EndpointProfile::Embedding => "/embeddings",
    };
    let mut headers = vec![
        ("Content-Type".into(), "application/json".into()),
        (
            "Accept".into(),
            if response_mode == ResponseMode::Streaming
                || profile == EndpointProfile::CodexSubscriptionResponses
            {
                "text/event-stream"
            } else {
                "application/json"
            }
            .into(),
        ),
    ];
    use crate::services::router::connections::profiles;
    if matches!(
        profile,
        EndpointProfile::AnthropicMessages | EndpointProfile::ClaudeSubscriptionMessages
    ) {
        headers.push((
            "anthropic-version".into(),
            profiles::ANTHROPIC_VERSION.into(),
        ));
    }
    if profile == EndpointProfile::ClaudeSubscriptionMessages
        && !required_betas
            .iter()
            .any(|v| v == profiles::CLAUDE_OAUTH_BETA)
    {
        required_betas.push(profiles::CLAUDE_OAUTH_BETA.into());
    }
    if !required_betas.is_empty() {
        headers.push(("anthropic-beta".into(), required_betas.join(",")));
    }
    if profile == EndpointProfile::CodexSubscriptionResponses {
        headers.push(("OpenAI-Beta".into(), "responses=experimental".into()));
        headers.push(("originator".into(), profiles::CODEX_ORIGINATOR.into()));
        headers.push(("User-Agent".into(), profiles::CODEX_ORIGINATOR.into()));
        if let Some(key) = &session_key {
            headers.push(("session_id".into(), key.clone()));
        }
    }
    Ok(PreparedRequest {
        identity,
        profile,
        path,
        body: bytes,
        headers,
        response_mode,
        tools,
        embedding_dimensions,
        session_key,
        artifact_refs: tracked.references.into_inner(),
    })
}
fn require(needed: bool, supported: bool, name: &'static str) -> Result<(), ProviderError> {
    if needed && !supported {
        Err(ProviderError::Unsupported(name))
    } else {
        Ok(())
    }
}
fn check(
    c: &ProviderContext,
    r: &InferenceRequest,
    caps: &Capabilities,
    images: bool,
    parallel: bool,
) -> Result<(), ProviderError> {
    let p = &c.parameters;
    if let Some(betas) = &p.anthropic_betas {
        let messages = matches!(
            caps.binding.endpoint_profile,
            EndpointProfile::AnthropicMessages | EndpointProfile::ClaudeSubscriptionMessages
        );
        let mut seen = std::collections::BTreeSet::new();
        for beta in betas {
            if beta.is_empty()
                || beta.len() > 128
                || !beta
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
                || !seen.insert(beta)
            {
                return Err(ProviderError::InvalidInput("anthropic_beta"));
            }
            let oauth = beta == crate::services::router::connections::profiles::CLAUDE_OAUTH_BETA;
            if !messages
                || (oauth
                    && caps.binding.endpoint_profile != EndpointProfile::ClaudeSubscriptionMessages)
                || (!oauth && !caps.anthropic_betas.contains(beta))
            {
                return Err(ProviderError::Unsupported("anthropic_beta"));
            }
        }
    }

    let has_images=c.messages.iter().any(|m|m.content.iter().any(|b|matches!(b,ContentBlock::Image{..})||matches!(b,ContentBlock::ToolResult{content,..} if content.iter().any(|v|matches!(v,ResultBlock::Image{..})))));
    let has_calls = c.messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(
                b,
                ContentBlock::ToolCall { .. } | ContentBlock::ToolResult { .. }
            )
        })
    });
    let requested_tools =
        matches!(&r.input,InferenceInput::Generation{tools,..} if !tools.is_empty());
    if let Some(ToolChoice::Tool { wire_name }) = &p.tool_choice
        && !matches!(&r.input, InferenceInput::Generation {tools,..} if tools.iter().any(|t| &t.wire_name == wire_name))
    {
        return Err(ProviderError::InvalidInput("unknown_tool_choice"));
    }
    require(images || has_images, caps.images.supported(), "images")?;
    require(
        has_calls || requested_tools || p.tool_choice.is_some(),
        caps.tools.supported(),
        "tools",
    )?;
    require(
        parallel || p.parallel_tool_calls == Some(true),
        caps.parallel_tools.supported(),
        "parallel_tools",
    )?;
    require(
        r.options.output_schema.is_some(),
        caps.structured_output.supported(),
        "structured_output",
    )?;
    require(
        p.temperature.is_some() || p.top_p.is_some(),
        caps.sampling.supported(),
        "sampling",
    )?;
    require(p.top_k.is_some(), caps.top_k.supported(), "top_k")?;
    require(p.stop.is_some(), caps.stop.supported(), "stop")?;
    effort::normalize_effort(p, &r.options, caps)?;
    require(
        p.prompt_cache_key.is_some(),
        caps.prompt_cache_key.supported(),
        "prompt_cache_key",
    )?;
    require(
        p.prompt_cache_retention.is_some(),
        caps.prompt_cache_retention.supported(),
        "prompt_cache_retention",
    )?;
    if p.prompt_cache_retention
        .as_ref()
        .is_some_and(|v| !caps.cache_retentions.contains(v))
    {
        return Err(ProviderError::Unsupported("cache_retention"));
    }
    let breakpoints = c
        .instructions
        .iter()
        .filter_map(|b| b.cache.as_ref())
        .chain(
            c.messages
                .iter()
                .flat_map(|m| m.content.iter())
                .filter_map(|b| match b {
                    ContentBlock::Text { cache, .. }
                    | ContentBlock::Image { cache, .. }
                    | ContentBlock::ToolResult { cache, .. } => cache.as_ref(),
                    _ => None,
                }),
        )
        .chain(p.tool_cache.iter().flat_map(|map| map.values()))
        .collect::<Vec<_>>();
    require(
        !breakpoints.is_empty(),
        caps.cache_breakpoints.supported(),
        "cache_breakpoints",
    )?;
    if let Some(cache) = &p.tool_cache {
        require(
            !cache.is_empty(),
            caps.cache_tool_breakpoints.supported(),
            "tool_cache_breakpoints",
        )?;
        if cache.keys().any(|name| !matches!(&r.input,InferenceInput::Generation{tools,..} if tools.iter().any(|t| &t.wire_name==name))) {
            return Err(ProviderError::InvalidInput("tool_cache_alias"));
        }
    }
    if caps
        .cache_maximum_breakpoints
        .is_some_and(|max| breakpoints.len() > max)
    {
        return Err(ProviderError::Unsupported("cache_breakpoint_count"));
    }
    if breakpoints.iter().any(|b| {
        b.ttl
            .as_ref()
            .is_some_and(|v| !caps.cache_breakpoint_ttls.contains(v))
    }) {
        return Err(ProviderError::Unsupported("cache_breakpoint_ttl"));
    }
    if matches!(
        caps.binding.endpoint_profile,
        EndpointProfile::OpenAiResponses | EndpointProfile::CodexSubscriptionResponses
    ) && !breakpoints.is_empty()
    {
        return Err(ProviderError::Unsupported("responses_cache_breakpoint"));
    }
    // These are endpoint constraints even if an unrelated declaration claims support.
    match caps.binding.endpoint_profile {
        EndpointProfile::CodexSubscriptionResponses => {
            if p.temperature.is_some()
                || p.top_p.is_some()
                || p.top_k.is_some()
                || p.stop.is_some()
                || p.max_output_tokens.is_some()
                || p.user_id.is_some()
            {
                return Err(ProviderError::Unsupported("codex_parameter"));
            }
        }
        EndpointProfile::OpenAiResponses => {
            if p.top_k.is_some() || p.stop.is_some() {
                return Err(ProviderError::Unsupported("responses_parameter"));
            }
        }
        EndpointProfile::AnthropicMessages | EndpointProfile::ClaudeSubscriptionMessages => {
            if p.temperature.is_some_and(|v| v > 1.0) {
                return Err(ProviderError::InvalidInput("messages_temperature"));
            }
            if p.prompt_cache_key.is_some() || p.prompt_cache_retention.is_some() {
                return Err(ProviderError::Unsupported("messages_cache_key"));
            }
        }
        EndpointProfile::CompatibleChat | EndpointProfile::OpenRouterChat => {
            if p.top_k.is_some() {
                return Err(ProviderError::Unsupported("chat_parameter"));
            }
        }
        EndpointProfile::Embedding => return Err(ProviderError::Unsupported("generation")),
    }
    Ok(())
}
// Serialization stops at the bound; neither Vec growth nor the JSON writer can
// materialize an oversized request just to discover its final size.
fn bounded_json(body: &Value, max: usize) -> Result<Vec<u8>, ProviderError> {
    struct Output {
        bytes: Vec<u8>,
        max: usize,
        exceeded: bool,
    }
    impl std::io::Write for Output {
        fn write(&mut self, value: &[u8]) -> std::io::Result<usize> {
            let Some(end) = self
                .bytes
                .len()
                .checked_add(value.len())
                .filter(|n| *n <= self.max)
            else {
                self.exceeded = true;
                return Err(std::io::Error::other("request size bound"));
            };
            if end > self.bytes.capacity() {
                let capacity = self
                    .bytes
                    .capacity()
                    .saturating_mul(2)
                    .max(end)
                    .min(self.max);
                self.bytes.reserve_exact(capacity - self.bytes.len());
            }
            self.bytes.extend_from_slice(value);
            Ok(value.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Output {
        bytes: Vec::with_capacity(max.min(64 * 1024)),
        max,
        exceeded: false,
    };
    if serde_json::to_writer(&mut output, body).is_err() {
        return Err(if output.exceeded {
            ProviderError::TooLarge
        } else {
            ProviderError::InvalidInput("request_json")
        });
    }
    Ok(output.bytes)
}

impl Encoder<'_> {
    fn charge_json<T: serde::Serialize + ?Sized>(&self, value: &T) -> Result<(), ProviderError> {
        struct Size {
            bytes: usize,
            max: usize,
        }
        impl std::io::Write for Size {
            fn write(&mut self, value: &[u8]) -> std::io::Result<usize> {
                self.bytes = self
                    .bytes
                    .checked_add(value.len())
                    .filter(|n| *n <= self.max)
                    .ok_or_else(|| std::io::Error::other("expanded request bound"))?;
                Ok(value.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut size = Size {
            bytes: 0,
            max: self.limits.request_bytes - self.expanded_artifact_bytes.get(),
        };
        serde_json::to_writer(&mut size, value).map_err(|_| ProviderError::TooLarge)?;
        self.expanded_artifact_bytes
            .set(self.expanded_artifact_bytes.get() + size.bytes);
        Ok(())
    }
    fn text(&self, reference: &ArtifactRef, error: &'static str) -> Result<String, ProviderError> {
        let text = String::from_utf8(
            self.materials
                .artifact(reference, self.limits.artifact_bytes)?,
        )
        .map_err(|_| ProviderError::InvalidInput(error))?;
        self.charge_json(&text)?;
        Ok(text)
    }
    fn arguments(&self, reference: &ArtifactRef) -> Result<String, ProviderError> {
        let text = self.text(reference, "tool_arguments")?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| ProviderError::InvalidInput("tool_arguments"))?;
        if !value.is_object() {
            return Err(ProviderError::InvalidInput("tool_arguments"));
        }
        Ok(text)
    }
    fn json(&self, r: &ArtifactRef) -> Result<Value, ProviderError> {
        let value =
            serde_json::from_slice(&self.materials.artifact(r, self.limits.artifact_bytes)?)
                .map_err(|_| ProviderError::InvalidInput("artifact_json"))?;
        self.charge_json(&value)?;
        Ok(value)
    }
    fn image(&self, r: &ArtifactRef) -> Result<(String, String), ProviderError> {
        if !matches!(
            r.media_type.as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        ) {
            return Err(ProviderError::Unsupported("image_media_type"));
        }
        let raw = u64::try_from(r.byte_length.get()).map_err(|_| ProviderError::TooLarge)?;
        let encoded = raw
            .checked_add(2)
            .and_then(|n| (n / 3).checked_mul(4))
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(ProviderError::TooLarge)?;
        // Charge every occurrence, even repeated refs to the same authorized blob.
        // The final bounded writer accounts for all profile-specific JSON overhead.
        let expanded = self
            .expanded_artifact_bytes
            .get()
            .checked_add(encoded)
            .and_then(|n| n.checked_add(r.media_type.len() + 32))
            .filter(|n| *n <= self.limits.request_bytes)
            .ok_or(ProviderError::TooLarge)?;
        self.expanded_artifact_bytes.set(expanded);
        let bytes = self
            .materials
            .artifact(r, self.limits.artifact_bytes.min(raw))?;
        if bytes.len() as u64 != raw {
            return Err(ProviderError::InvalidInput("artifact_length"));
        }
        Ok((r.media_type.clone(), base64(&bytes)))
    }

    fn native(
        &self,
        b: &crate::protocol::provider::NativeBinding,
        a: &ArtifactRef,
    ) -> Result<Value, ProviderError> {
        if b != &self.identity.binding {
            return Err(ProviderError::ForeignNative);
        }
        let v: Value =
            serde_json::from_slice(&self.materials.native(a, b, self.limits.artifact_bytes)?)
                .map_err(|_| ProviderError::InvalidInput("native_item"))?;
        if !v.is_object() {
            return Err(ProviderError::InvalidInput("native_item"));
        }
        self.charge_json(&v)?;
        Ok(v)
    }
    fn generation(
        &self,
        c: &ProviderContext,
        r: &InferenceRequest,
        tools: &[PreparedTool],
        headroom: i64,
        constraints: Option<&OpenRouterConstraints>,
        caps: &Capabilities,
    ) -> Result<Value, ProviderError> {
        let profile = &self.identity.binding.endpoint_profile;
        let p = &c.parameters;
        let streaming =
            p.response_mode.unwrap_or(ResponseMode::Streaming) == ResponseMode::Streaming;
        let max = p.max_output_tokens.map(i64::from).unwrap_or(headroom);
        if max > headroom {
            return Err(ProviderError::InvalidInput("response_headroom"));
        }
        let mut body = json!({"model":self.identity.upstream_model,"stream":streaming});
        match profile {
            EndpointProfile::AnthropicMessages | EndpointProfile::ClaudeSubscriptionMessages => {
                body["max_tokens"] = json!(max);
                let mut system = c
                    .instructions
                    .iter()
                    .map(|t| cache(json!({"type":"text","text":t.text}), t.cache.as_ref()))
                    .collect::<Vec<_>>();
                if profile == &EndpointProfile::ClaudeSubscriptionMessages {
                    let identity = "You are Claude Code, Anthropic's official CLI for Claude.";
                    if !system.first().is_some_and(|v| {
                        v["text"]
                            .as_str()
                            .is_some_and(|v| v.trim_start().starts_with(identity))
                    }) {
                        system.insert(0, json!({"type":"text","text":identity}));
                    }
                }
                if !system.is_empty() {
                    body["system"] = json!(system);
                }
                body["messages"] = json!(
                    c.messages
                        .iter()
                        .map(|m| self.message(m))
                        .collect::<Result<Vec<_>, _>>()?
                );
                if !tools.is_empty() {
                    body["tools"] = json!(
                        tools
                            .iter()
                            .map(|t| cache(
                                tool_definition(t, ToolDialect::Messages),
                                p.tool_cache.as_ref().and_then(|map| map.get(&t.wire_name))
                            ))
                            .collect::<Vec<_>>()
                    );
                }
                if let Some(v) = p.temperature {
                    body["temperature"] = json!(v);
                }
                if let Some(v) = p.top_p {
                    body["top_p"] = json!(v);
                }
                if let Some(v) = p.top_k {
                    body["top_k"] = json!(v);
                }
                if let Some(v) = &p.stop {
                    body["stop_sequences"] = json!(v);
                }
                if let Some(v) = &p.user_id {
                    body["metadata"] = json!({"user_id":v});
                }
                if let Some(v) = &p.thinking {
                    body["thinking"] = match v {
                        Thinking::Disabled => json!({"type":"disabled"}),
                        Thinking::Enabled { budget_tokens } => {
                            json!({"type":"enabled","budget_tokens":budget_tokens})
                        }
                        Thinking::Adaptive => json!({"type":"adaptive"}),
                    };
                }
                if p.tool_choice.is_some() || p.parallel_tool_calls.is_some() {
                    let mut choice = match &p.tool_choice {
                        None | Some(ToolChoice::Auto) => json!({"type":"auto"}),
                        Some(ToolChoice::Any) => json!({"type":"any"}),
                        Some(ToolChoice::None) => json!({"type":"none"}),
                        Some(ToolChoice::Tool { wire_name }) => {
                            json!({"type":"tool","name":wire_name})
                        }
                    };
                    if let Some(v) = p.parallel_tool_calls {
                        choice["disable_parallel_tool_use"] = json!(!v);
                    }
                    body["tool_choice"] = choice;
                }
                if let Some(schema) = &r.options.output_schema {
                    body["output_config"]["format"] = json!({"type":"json_schema","schema":materialize::schema(self.materials, schema)?});
                }
            }
            EndpointProfile::CompatibleChat | EndpointProfile::OpenRouterChat => {
                let mut messages = Vec::new();
                if !c.instructions.is_empty() {
                    let content = if c.instructions.iter().any(|t| t.cache.is_some()) {
                        json!(
                            c.instructions
                                .iter()
                                .map(|t| cache(
                                    json!({"type":"text","text":t.text}),
                                    t.cache.as_ref()
                                ))
                                .collect::<Vec<_>>()
                        )
                    } else {
                        json!(
                            c.instructions
                                .iter()
                                .map(|t| t.text.as_str())
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        )
                    };
                    messages.push(json!({"role":"system","content":content}));
                }
                for m in &c.messages {
                    self.chat(m, &mut messages)?;
                }
                body["messages"] = json!(messages);
                body["max_tokens"] = json!(max);
                if !tools.is_empty() {
                    body["tools"] = json!(
                        tools
                            .iter()
                            .map(|t| cache(
                                tool_definition(t, ToolDialect::Chat),
                                p.tool_cache.as_ref().and_then(|map| map.get(&t.wire_name))
                            ))
                            .collect::<Vec<_>>()
                    );
                }
                if streaming {
                    body["stream_options"] = json!({"include_usage":true});
                }
                if profile == &EndpointProfile::OpenRouterChat
                    && let InferenceInput::Generation { session_id, .. } = &r.input
                {
                    body["session_id"] = json!(crate::fingerprint::sha256_hex(
                        session_id.as_str().as_bytes()
                    ));
                }
                if matches!(p.thinking, Some(Thinking::Adaptive)) && r.options.effort.is_none() {
                    body["reasoning"] = json!({"enabled":true,"exclude":false});
                }
                if let Some(v) = p.temperature {
                    body["temperature"] = json!(v);
                }
                if let Some(v) = p.top_p {
                    body["top_p"] = json!(v);
                }
                if let Some(v) = &p.stop {
                    body["stop"] = json!(v);
                }
                if let Some(v) = &p.user_id {
                    body["user"] = json!(v);
                }
                if let Some(v) = p.parallel_tool_calls {
                    body["parallel_tool_calls"] = json!(v);
                }
                if let Some(v) = &p.tool_choice {
                    body["tool_choice"] = chat_choice(v);
                }
                if let Some(schema) = &r.options.output_schema {
                    body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"response","strict":true,"schema":materialize::schema(self.materials, schema)?}});
                }
                if profile == &EndpointProfile::OpenRouterChat
                    && let Some(constraints) = constraints
                {
                    for (k, v) in crate::services::router::connections::open_router_request_fields(
                        constraints,
                        &self.identity.upstream_model,
                    ) {
                        body[k] = v;
                    }
                }
            }
            EndpointProfile::OpenAiResponses | EndpointProfile::CodexSubscriptionResponses => {
                body["instructions"] = json!(
                    c.instructions
                        .iter()
                        .map(|t| t.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n")
                );
                let mut input = Vec::new();
                for m in &c.messages {
                    self.responses(m, &mut input)?;
                }
                body["input"] = json!(input);
                if profile == &EndpointProfile::CodexSubscriptionResponses {
                    body["stream"] = json!(true);
                    body["store"] = json!(false);
                    body["include"] = json!(["reasoning.encrypted_content"]);
                    body["reasoning"] = json!({"summary":"auto"});
                    if let InferenceInput::Generation { session_id, .. } = &r.input {
                        body["prompt_cache_key"] = json!(crate::fingerprint::sha256_hex(
                            session_id.as_str().as_bytes()
                        ));
                    }
                } else {
                    body["max_output_tokens"] = json!(max);
                    if let Some(v) = p.temperature {
                        body["temperature"] = json!(v);
                    }
                    if let Some(v) = p.top_p {
                        body["top_p"] = json!(v);
                    }
                    if let Some(v) = &p.user_id {
                        body["user"] = json!(v);
                    }
                }
                if !tools.is_empty() {
                    body["tools"] = json!(
                        tools
                            .iter()
                            .map(|t| tool_definition(t, ToolDialect::Responses))
                            .collect::<Vec<_>>()
                    );
                }
                if let Some(v) = p.parallel_tool_calls {
                    body["parallel_tool_calls"] = json!(v);
                }
                if let Some(v) = &p.tool_choice {
                    body["tool_choice"] = match v {
                        ToolChoice::Tool { wire_name } => {
                            json!({"type":"function","name":wire_name})
                        }
                        _ => chat_choice(v),
                    };
                }
                if let Some(schema) = &r.options.output_schema {
                    body["text"]["format"] = json!({"type":"json_schema","name":"response","strict":true,"schema":materialize::schema(self.materials, schema)?});
                }
            }
            EndpointProfile::Embedding => unreachable!("generation checked"),
        }
        if let Some(v) = &p.prompt_cache_key {
            body["prompt_cache_key"] = json!(v);
        }
        if let Some(v) = &p.prompt_cache_retention {
            body["prompt_cache_retention"] = json!(v);
        }
        if let Some(encoding) = effort::normalize_effort(p, &r.options, caps)?.encoding {
            match &encoding {
                EffortEncoding::ReasoningEffort { value }
                    if profile == &EndpointProfile::CompatibleChat =>
                {
                    body["reasoning_effort"] = json!(value)
                }
                EffortEncoding::ReasoningEffort { value }
                    if profile == &EndpointProfile::OpenRouterChat =>
                {
                    body["reasoning"] = json!({"effort":value,"exclude":false})
                }
                EffortEncoding::ChatTemplateKwargs { value }
                    if profile == &EndpointProfile::CompatibleChat =>
                {
                    body["chat_template_kwargs"] = value.clone()
                }
                EffortEncoding::MessagesOutputConfig { value }
                    if matches!(
                        profile,
                        EndpointProfile::AnthropicMessages
                            | EndpointProfile::ClaudeSubscriptionMessages
                    ) =>
                {
                    body["output_config"]["effort"] = json!(value)
                }
                EffortEncoding::ResponsesReasoning { value }
                    if matches!(
                        profile,
                        EndpointProfile::OpenAiResponses
                            | EndpointProfile::CodexSubscriptionResponses
                    ) =>
                {
                    body["reasoning"]["effort"] = json!(value)
                }
                _ => return Err(ProviderError::Unsupported("effort_encoding")),
            }
        }
        Ok(body)
    }
    fn message(&self, m: &Message) -> Result<Value, ProviderError> {
        let mut content = Vec::new();
        for b in &m.content {
            content.push(match b{
                ContentBlock::Text{text,cache:c}=>cache(json!({"type":"text","text":text}),c.as_ref()),
                ContentBlock::Image{artifact,detail,cache:c}=>{
                    if detail.is_some(){return Err(ProviderError::Unsupported("messages_image_detail"));}
                    let (media_type,data)=self.image(artifact)?;
                    cache(json!({"type":"image","source":{"type":"base64","media_type":media_type,"data":data}}),c.as_ref())
                }
                ContentBlock::ToolCall{call_id,wire_name,arguments}=>{
                    if !matches!(m.role,MessageRole::Assistant){return Err(ProviderError::InvalidInput("tool_call_role"));}
                    let args=self.json(arguments)?;if !args.is_object(){return Err(ProviderError::InvalidInput("tool_arguments"));}
                    json!({"type":"tool_use","id":call_id,"name":wire_name,"input":args})
                }
                ContentBlock::ToolResult{call_id,content,is_error,cache:c,..}=>{
                    if !matches!(m.role,MessageRole::User){return Err(ProviderError::InvalidInput("tool_result_role"));}
                    let blocks=content.iter().map(|v|match v{
                        ResultBlock::Text{text}=>Ok(json!({"type":"text","text":text})),
                        ResultBlock::Image{artifact,detail}=>{
                            if detail.is_some(){return Err(ProviderError::Unsupported("messages_image_detail"));}
                            let(media_type,data)=self.image(artifact)?;
                            Ok(json!({"type":"image","source":{"type":"base64","media_type":media_type,"data":data}}))
                        }
                    }).collect::<Result<Vec<_>,_>>()?;
                    cache(json!({"type":"tool_result","tool_use_id":call_id,"content":blocks,"is_error":is_error}),c.as_ref())
                }
                ContentBlock::Native{binding,artifact}=>{
                    if !matches!(m.role,MessageRole::Assistant){return Err(ProviderError::InvalidInput("native_role"));}
                    let v=self.native(binding,artifact)?;
                    if !matches!(v["type"].as_str(),Some("thinking"|"redacted_thinking")){return Err(ProviderError::InvalidInput("messages_native"));}
                    v
                }
            });
        }
        Ok(json!({"role":role(&m.role),"content":content}))
    }
    fn result_images(
        &self,
        content: &[ResultBlock],
        responses: bool,
    ) -> Result<Vec<Value>, ProviderError> {
        content
            .iter()
            .filter_map(|v| match v {
                ResultBlock::Text { .. } => None,
                ResultBlock::Image { artifact, detail } => Some((artifact, detail)),
            })
            .map(|(artifact, detail)| {
                let (media, data) = self.image(artifact)?;
                let url = format!("data:{media};base64,{data}");
                let mut v = if responses {
                    json!({"type":"input_image","image_url":url})
                } else {
                    json!({"type":"image_url","image_url":{"url":url}})
                };
                if let Some(detail) = detail {
                    if responses {
                        v["detail"] = json!(detail_name(detail));
                    } else {
                        v["image_url"]["detail"] = json!(detail_name(detail));
                    }
                }
                Ok(v)
            })
            .collect()
    }
    fn chat(&self, m: &Message, out: &mut Vec<Value>) -> Result<(), ProviderError> {
        let mut parts = Vec::new();
        let mut calls = Vec::new();
        let mut native = serde_json::Map::new();
        let mut result_images = Vec::new();
        for b in &m.content {
            if !matches!(b, ContentBlock::ToolResult { .. }) {
                flush_result_images(&mut result_images, out);
            }
            match b {
                ContentBlock::Text { text, cache: c } => {
                    parts.push(cache(json!({"type":"text","text":text}), c.as_ref()))
                }
                ContentBlock::Image {
                    artifact,
                    detail,
                    cache: c,
                } => {
                    if !matches!(m.role, MessageRole::User) {
                        return Err(ProviderError::Unsupported("chat_assistant_image"));
                    }
                    let (media, data) = self.image(artifact)?;
                    let mut v = json!({"type":"image_url","image_url":{"url":format!("data:{media};base64,{data}")}});
                    if let Some(d) = detail {
                        v["image_url"]["detail"] = json!(detail_name(d));
                    }
                    parts.push(cache(v, c.as_ref()));
                }
                ContentBlock::ToolCall {
                    call_id,
                    wire_name,
                    arguments,
                } => {
                    if !matches!(m.role, MessageRole::Assistant) {
                        return Err(ProviderError::InvalidInput("tool_call_role"));
                    }
                    let raw = self.arguments(arguments)?;
                    calls.push(json!({"id":call_id,"type":"function","function":{"name":wire_name,"arguments":raw}}));
                }
                ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error,
                    cache: c,
                    ..
                } => {
                    if !matches!(m.role, MessageRole::User) {
                        return Err(ProviderError::InvalidInput("tool_result_role"));
                    }
                    flush_chat(m, &mut parts, &mut calls, &mut native, out);
                    result_images.extend(self.result_images(content, false)?);
                    let text = result_text(content, *is_error)?;
                    let content = if c.is_some() {
                        json!([cache(json!({"type":"text","text":text}), c.as_ref())])
                    } else {
                        json!(text)
                    };
                    out.push(json!({"role":"tool","tool_call_id":call_id,"content":content}));
                }
                ContentBlock::Native { binding, artifact } => {
                    if !matches!(m.role, MessageRole::Assistant) {
                        return Err(ProviderError::InvalidInput("native_role"));
                    }
                    let v = self.native(binding, artifact)?;
                    for (k, v) in v.as_object().expect("object") {
                        if !matches!(
                            k.as_str(),
                            "reasoning" | "reasoning_content" | "reasoning_details"
                        ) || native.contains_key(k)
                        {
                            return Err(ProviderError::InvalidInput("chat_native"));
                        }
                        native.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        flush_result_images(&mut result_images, out);
        flush_chat(m, &mut parts, &mut calls, &mut native, out);
        Ok(())
    }
    fn responses(&self, m: &Message, out: &mut Vec<Value>) -> Result<(), ProviderError> {
        let mut parts = Vec::new();
        let mut result_images = Vec::new();
        for b in &m.content {
            if !matches!(b, ContentBlock::ToolResult { .. }) {
                flush_result_images(&mut result_images, out);
            }
            match b {
                ContentBlock::Text{text,..}=>parts.push(json!({"type":if matches!(m.role,MessageRole::Assistant){"output_text"}else{"input_text"},"text":text})),
                ContentBlock::Image{artifact,detail,..}=>{
                    if !matches!(m.role,MessageRole::User){return Err(ProviderError::Unsupported("responses_assistant_image"));}
                    let(media,data)=self.image(artifact)?;let mut v=json!({"type":"input_image","image_url":format!("data:{media};base64,{data}")});
                    if let Some(d)=detail{v["detail"]=json!(detail_name(d));}parts.push(v);
                }
                ContentBlock::ToolCall{call_id,wire_name,arguments}=>{
                    if !matches!(m.role,MessageRole::Assistant){return Err(ProviderError::InvalidInput("tool_call_role"));}
                    flush_responses(m,&mut parts,out);
                    let raw=self.arguments(arguments)?;
                    out.push(json!({"type":"function_call","call_id":call_id,"name":wire_name,"arguments":raw}));
                }
                ContentBlock::ToolResult{call_id,content,is_error,..}=>{
                    if !matches!(m.role,MessageRole::User){return Err(ProviderError::InvalidInput("tool_result_role"));}
                    flush_responses(m,&mut parts,out);
                    result_images.extend(self.result_images(content,true)?);
                    out.push(json!({"type":"function_call_output","call_id":call_id,"output":result_text(content,*is_error)?}));
                }
                ContentBlock::Native{binding,artifact}=>{
                    if !matches!(m.role,MessageRole::Assistant){return Err(ProviderError::InvalidInput("native_role"));}
                    flush_responses(m,&mut parts,out);let v=self.native(binding,artifact)?;
                    if !matches!(v["type"].as_str(),Some("reasoning"|"compaction")){return Err(ProviderError::InvalidInput("responses_native"));}out.push(v);
                }
            }
        }
        flush_result_images(&mut result_images, out);
        flush_responses(m, &mut parts, out);
        Ok(())
    }
}
fn flush_chat(
    m: &Message,
    parts: &mut Vec<Value>,
    calls: &mut Vec<Value>,
    native: &mut serde_json::Map<String, Value>,
    out: &mut Vec<Value>,
) {
    if parts.is_empty() && calls.is_empty() && native.is_empty() {
        return;
    }
    let content = if parts.is_empty() {
        Value::Null
    } else if parts
        .iter()
        .all(|p| p["type"] == "text" && p.get("cache_control").is_none())
    {
        json!(
            parts
                .iter()
                .map(|p| p["text"].as_str().expect("encoded text"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        json!(parts)
    };
    parts.clear();
    let mut v = json!({"role":role(&m.role),"content":content});
    if !calls.is_empty() {
        v["tool_calls"] = json!(std::mem::take(calls));
    }
    for (k, value) in std::mem::take(native) {
        v[k] = value;
    }
    out.push(v);
}
fn flush_responses(m: &Message, parts: &mut Vec<Value>, out: &mut Vec<Value>) {
    if !parts.is_empty() {
        out.push(json!({"role":role(&m.role),"content":std::mem::take(parts)}));
    }
}
fn role(r: &MessageRole) -> &'static str {
    match r {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
    }
}
fn detail_name(d: &ImageDetail) -> &'static str {
    match d {
        ImageDetail::Auto => "auto",
        ImageDetail::Low => "low",
        ImageDetail::High => "high",
    }
}
fn chat_choice(c: &ToolChoice) -> Value {
    match c {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::Any => json!("required"),
        ToolChoice::None => json!("none"),
        ToolChoice::Tool { wire_name } => json!({"type":"function","function":{"name":wire_name}}),
    }
}
fn cache(mut v: Value, b: Option<&CacheBreakpoint>) -> Value {
    if let Some(b) = b {
        v["cache_control"] = json!({"type":"ephemeral"});
        if let Some(ttl) = &b.ttl {
            v["cache_control"]["ttl"] = json!(ttl);
        }
    }
    v
}
fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for b in bytes.chunks(3) {
        let n = (u32::from(b[0]) << 16)
            | (u32::from(*b.get(1).unwrap_or(&0)) << 8)
            | u32::from(*b.get(2).unwrap_or(&0));
        out.push(A[((n >> 18) & 63) as usize] as char);
        out.push(A[((n >> 12) & 63) as usize] as char);
        out.push(if b.len() > 1 {
            A[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if b.len() > 2 {
            A[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn result_text(content: &[ResultBlock], is_error: bool) -> Result<String, ProviderError> {
    if is_error {
        return serde_json::to_string(&json!({"is_error":true,"content":content}))
            .map_err(|_| ProviderError::InvalidInput("tool_result_json"));
    }
    Ok(content
        .iter()
        .filter_map(|v| match v {
            ResultBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n"))
}
fn flush_result_images(images: &mut Vec<Value>, out: &mut Vec<Value>) {
    if !images.is_empty() {
        out.push(json!({"role":"user","content":std::mem::take(images)}));
    }
}

enum ToolDialect {
    Messages,
    Chat,
    Responses,
}
fn tool_definition(tool: &PreparedTool, dialect: ToolDialect) -> Value {
    let mut definition = match dialect {
        ToolDialect::Messages => json!({"name":tool.wire_name,"input_schema":tool.schema}),
        ToolDialect::Chat | ToolDialect::Responses => {
            json!({"name":tool.wire_name,"parameters":tool.schema})
        }
    };
    if let Some(description) = &tool.description {
        definition["description"] = json!(description);
    }
    match dialect {
        ToolDialect::Messages => definition,
        ToolDialect::Chat => json!({"type":"function","function":definition}),
        ToolDialect::Responses => {
            definition["type"] = json!("function");
            definition
        }
    }
}
