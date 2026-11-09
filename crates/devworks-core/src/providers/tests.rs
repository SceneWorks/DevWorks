use super::capabilities::Support;
use super::{capabilities::*, context::*, encode::*, materialize::Materials, *};
use crate::protocol::provider::*;
use crate::protocol::{Counter, Id, Revision};
use serde_json::{Value, json};
use std::collections::BTreeMap;
fn id(s: &str) -> Id {
    Id::new(s).unwrap()
}
struct Material {
    blobs: BTreeMap<String, Vec<u8>>,
    schema: Value,
    reads: std::cell::Cell<usize>,
    tool_reads: std::cell::Cell<usize>,
    tool_description: String,
}
impl Material {
    fn new() -> Self {
        Self {
            blobs: BTreeMap::new(),
            reads: std::cell::Cell::new(0),
            tool_reads: std::cell::Cell::new(0),
            tool_description: "Look up the exact current record.".into(),
            schema: json!({"type":"object","properties":{"x":{"type":"string"}},"additionalProperties":false}),
        }
    }
    fn put(&mut self, bytes: &[u8], media: &str) -> ArtifactRef {
        let sha = crate::fingerprint::sha256_hex(bytes);
        self.blobs.insert(sha.clone(), bytes.to_vec());
        ArtifactRef {
            artifact_id: id("artifact"),
            version_id: id("version"),
            sha256: sha.try_into().unwrap(),
            byte_length: Counter::new(bytes.len() as i64).unwrap(),
            media_type: media.into(),
        }
    }
    fn schema_ref(&self) -> SchemaRef {
        SchemaRef {
            schema_id: "test/input".to_owned().try_into().unwrap(),
            revision: Revision::new(1).unwrap(),
            sha256: crate::fingerprint::sha256_hex(
                crate::jcs::canonicalize(&self.schema).unwrap().as_bytes(),
            )
            .try_into()
            .unwrap(),
        }
    }
}
impl Materials for Material {
    fn artifact(&self, r: &ArtifactRef, max: u64) -> Result<Vec<u8>, ProviderError> {
        self.reads.set(self.reads.get() + 1);
        let b = self
            .blobs
            .get(r.sha256.as_str())
            .ok_or(ProviderError::MaterialUnavailable)?;
        if b.len() as u64 > max {
            return Err(ProviderError::TooLarge);
        }
        Ok(b.clone())
    }
    fn native(
        &self,
        reference: &ArtifactRef,
        _binding: &NativeBinding,
        max: u64,
    ) -> Result<Vec<u8>, ProviderError> {
        self.artifact(reference, max)
    }
    fn schema(&self, _: &SchemaRef) -> Result<Value, ProviderError> {
        Ok(self.schema.clone())
    }
    fn tool(&self, alias: &ToolSchema) -> Result<PreparedTool, ProviderError> {
        self.tool_reads.set(self.tool_reads.get() + 1);
        Ok(PreparedTool {
            wire_name: alias.wire_name.clone(),
            capability_id: alias.capability_id.clone(),
            input_schema: alias.input_schema.clone(),
            schema: self.schema.clone(),
            description: Some(self.tool_description.clone()),
        })
    }
}
fn context() -> ProviderContext {
    ProviderContext {
        version: 1,
        instructions: vec![TextBlock {
            text: "Rules".into(),
            cache: None,
        }],
        instruction_stamps: vec![],
        messages: vec![Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "Hello".into(),
                cache: None,
            }],
        }],
        parameters: GenerationParameters::default(),
    }
}
fn binding(profile: EndpointProfile) -> NativeBinding {
    NativeBinding {
        connection_id: id("connection"),
        account_revision: Revision::new(1).unwrap(),
        endpoint_profile: profile,
        model_id: id("catalog"),
        context_epoch: Counter::new(0).unwrap(),
    }
}
fn identity(b: NativeBinding) -> RequestIdentity {
    RequestIdentity {
        request_id: id("request"),
        attempt_id: crate::protocol::AttemptId(id("attempt")),
        binding: b,
        upstream_model: "exact-model-1-snapshot".into(),
    }
}
fn caps(b: NativeBinding) -> Capabilities {
    let mut c = Capabilities::unknown(b);
    c.images = Support::Supported;
    c.tools = Support::Supported;
    c.parallel_tools = Support::Supported;
    c.structured_output = Support::Supported;
    c.embeddings = Support::Supported;
    c.embedding_dimensions = EmbeddingDimensions::Parameter;
    c.sampling = Support::Supported;
    c.top_k = Support::Supported;
    c.stop = Support::Supported;
    c.thinking = Support::Supported;
    c.prompt_cache_key = Support::Supported;
    c.prompt_cache_retention = Support::Supported;
    c.cache_breakpoints = Support::Supported;
    c.cache_tool_breakpoints = Support::Supported;
    c.cache_minimum_input_tokens = Some(1024);
    c.cache_maximum_breakpoints = Some(4);
    c.cache_breakpoint_ttls = vec!["5m".into(), "1h".into()];
    c.cache_retentions = vec!["in_memory".into(), "24h".into()];
    c
}
fn request(m: &mut Material, c: &ProviderContext) -> InferenceRequest {
    let reference = m.put(
        &serde_json::to_vec(c).unwrap(),
        "application/vnd.devworks.provider-context+json",
    );
    serde_json::from_value(json!({"protocol":"1.0-draft.1","requestId":"request","purpose":"Conversation","attribution":{"kind":"Workspace","sessionId":"session"},"authorityRevision":"1","controlGeneration":"1","instructionRevision":"1","cancellationRevision":"1","options":{"selection":{"kind":"Model","modelId":"catalog"},"deadlineMs":30000},"input":{"kind":"Generation","sessionId":"session","context":reference,"contextEpoch":"0","tools":[],"responseHeadroomTokens":"1024","requiresImages":false,"requiresParallelTools":false}})).unwrap()
}
fn limits() -> PreparationLimits {
    PreparationLimits {
        context_bytes: 4 * 1024 * 1024,
        artifact_bytes: 4 * 1024 * 1024,
        request_bytes: 8 * 1024 * 1024,
    }
}
fn run(profile: EndpointProfile, c: &ProviderContext) -> PreparedRequest {
    let mut m = Material::new();
    let r = request(&mut m, c);
    let b = binding(profile);
    prepare(identity(b.clone()), &r, &caps(b), &m, None, &limits()).unwrap()
}
fn body(r: &PreparedRequest) -> Value {
    serde_json::from_slice(&r.body).unwrap()
}
#[test]
fn all_generation_profiles_encode_distinct_native_shapes_without_credentials() {
    for profile in [
        EndpointProfile::OpenAiResponses,
        EndpointProfile::CodexSubscriptionResponses,
        EndpointProfile::AnthropicMessages,
        EndpointProfile::ClaudeSubscriptionMessages,
        EndpointProfile::OpenRouterChat,
        EndpointProfile::CompatibleChat,
    ] {
        let p = run(profile.clone(), &context());
        let v = body(&p);
        assert_eq!(v["model"], "exact-model-1-snapshot");
        assert!(v.get("actorId").is_none());
        assert!(
            p.headers
                .iter()
                .all(|(k, _)| !k.eq_ignore_ascii_case("authorization")
                    && !k.eq_ignore_ascii_case("x-api-key"))
        );
        match profile {
            EndpointProfile::AnthropicMessages | EndpointProfile::ClaudeSubscriptionMessages => {
                assert_eq!(v["messages"][0]["content"][0]["text"], "Hello");
                assert!(
                    p.headers
                        .iter()
                        .any(|(k, v)| k == "anthropic-version" && v == "2023-06-01")
                );
                if profile == EndpointProfile::AnthropicMessages {
                    assert_eq!(v["system"].as_array().unwrap().len(), 1);
                    assert_eq!(v["system"][0]["text"], "Rules");
                    assert!(!p.headers.iter().any(|(k, _)| k == "anthropic-beta"));
                } else {
                    assert_eq!(v["system"].as_array().unwrap().len(), 2);
                    assert!(
                        v["system"][0]["text"]
                            .as_str()
                            .unwrap()
                            .starts_with("You are Claude Code")
                    );
                    assert!(
                        p.headers
                            .iter()
                            .any(|(k, v)| k == "anthropic-beta" && v == "oauth-2025-04-20")
                    );
                }
            }
            EndpointProfile::OpenAiResponses | EndpointProfile::CodexSubscriptionResponses => {
                assert_eq!(v["input"][0]["content"][0]["type"], "input_text");
                if profile == EndpointProfile::CodexSubscriptionResponses {
                    assert_eq!(v["store"], false);
                    assert_eq!(v["include"], json!(["reasoning.encrypted_content"]));
                    assert!(v.get("max_output_tokens").is_none());
                    assert_eq!(
                        p.session_key.as_ref().unwrap(),
                        v["prompt_cache_key"].as_str().unwrap()
                    );
                } else {
                    assert_eq!(v["max_output_tokens"], 1024);
                    assert!(v.get("store").is_none());
                }
            }
            _ => assert_eq!(v["messages"][1]["content"], "Hello"),
        }
    }
}
#[test]
fn embedding_batch_keeps_order_dimensions_and_exact_upstream() {
    let mut m = Material::new();
    let one = m.put("first🧪".as_bytes(), "text/plain");
    let two = m.put(b"second", "text/plain");
    let mut r = request(&mut m, &context());
    r.purpose = InferencePurpose::Embedding;
    r.input = InferenceInput::Embedding {
        embedding_space_id: id("space"),
        dimensions: 3,
        inputs: vec![one, two],
    };
    let b = binding(EndpointProfile::Embedding);
    let p = prepare(identity(b.clone()), &r, &caps(b), &m, None, &limits()).unwrap();
    assert_eq!(p.path, "/embeddings");
    assert_eq!(p.response_mode, ResponseMode::Aggregate);
    assert_eq!(p.embedding_dimensions, Some(3));
    assert_eq!(body(&p)["input"], json!(["first🧪", "second"]));
    assert_eq!(body(&p)["dimensions"], 3);
}
#[test]
fn retained_sampling_stop_tool_choice_parallel_and_cache_are_explicit() {
    let mut c = context();
    c.parameters.temperature = Some(0.4);
    c.parameters.top_p = Some(0.8);
    c.parameters.top_k = Some(7);
    c.parameters.stop = Some(vec!["DONE".into()]);
    c.parameters.parallel_tool_calls = Some(false);
    c.parameters.tool_choice = Some(ToolChoice::Auto);
    c.instructions[0].cache = Some(CacheBreakpoint {
        ttl: Some("1h".into()),
    });
    let p = run(EndpointProfile::AnthropicMessages, &c);
    let v = body(&p);
    assert_eq!(v["temperature"], 0.4);
    assert_eq!(v["top_p"], 0.8);
    assert_eq!(v["top_k"], 7);
    assert_eq!(v["stop_sequences"], json!(["DONE"]));
    assert_eq!(v["tool_choice"]["disable_parallel_tool_use"], true);
    assert_eq!(
        v["system"][0]["cache_control"],
        json!({"type":"ephemeral","ttl":"1h"})
    );
    let mut m = Material::new();
    let r = request(&mut m, &c);
    let b = binding(EndpointProfile::AnthropicMessages);
    let mut cap = caps(b.clone());
    cap.sampling = Support::Unknown;
    assert!(matches!(
        prepare(identity(b), &r, &cap, &m, None, &limits()),
        Err(ProviderError::Unsupported("sampling"))
    ));
}
#[test]
fn codex_refuses_api_only_controls_and_aggregates_through_streaming() {
    let mut c = context();
    c.parameters.response_mode = Some(ResponseMode::Aggregate);
    let p = run(EndpointProfile::CodexSubscriptionResponses, &c);
    assert_eq!(p.response_mode, ResponseMode::Aggregate);
    assert_eq!(body(&p)["stream"], true);
    c.parameters.temperature = Some(0.2);
    let mut m = Material::new();
    let r = request(&mut m, &c);
    let b = binding(EndpointProfile::CodexSubscriptionResponses);
    assert!(matches!(
        prepare(identity(b.clone()), &r, &caps(b), &m, None, &limits()),
        Err(ProviderError::Unsupported("codex_parameter"))
    ));
    assert_eq!(
        body(&run(EndpointProfile::OpenAiResponses, &c))["temperature"],
        0.2
    );
}
#[test]
fn exact_native_bindings_and_tool_argument_bytes_survive() {
    let mut m = Material::new();
    let args = m.put(b"{ \"x\" : \"hello\" }", "application/json");
    let opaque =
        json!({"type":"reasoning","id":"rs_1","encrypted_content":"opaque.signature","summary":[]});
    let artifact = m.put(&serde_json::to_vec(&opaque).unwrap(), "application/json");
    let b = binding(EndpointProfile::OpenAiResponses);
    let mut c = context();
    c.messages.push(Message {
        role: MessageRole::Assistant,
        content: vec![
            ContentBlock::Native {
                binding: b.clone(),
                artifact,
            },
            ContentBlock::Text {
                text: "Before".into(),
                cache: None,
            },
            ContentBlock::ToolCall {
                call_id: "call-a".into(),
                wire_name: "lookup".into(),
                arguments: args,
            },
        ],
    });
    let r = request(&mut m, &c);
    let p = prepare(
        identity(b.clone()),
        &r,
        &caps(b.clone()),
        &m,
        None,
        &limits(),
    )
    .unwrap();
    let v = body(&p);
    assert_eq!(p.artifact_refs.len(), 3); // context + opaque item + exact arguments
    assert!(
        p.artifact_refs
            .iter()
            .any(|r| r.sha256.as_str() == crate::fingerprint::sha256_hex(b"{ \"x\" : \"hello\" }"))
    );
    assert_eq!(v["input"][1], opaque);
    assert_eq!(v["input"][2]["content"][0]["text"], "Before");
    assert_eq!(v["input"][3]["call_id"], "call-a");
    assert_eq!(v["input"][3]["arguments"], "{ \"x\" : \"hello\" }");
    if let ContentBlock::Native { binding, .. } = &mut c.messages[1].content[0] {
        binding.account_revision = Revision::new(2).unwrap();
    }
    let r = request(&mut m, &c);
    assert!(matches!(
        prepare(identity(b.clone()), &r, &caps(b), &m, None, &limits()),
        Err(ProviderError::ForeignNative)
    ));
}
#[test]
fn image_artifacts_are_encoded_or_refused_never_omitted() {
    let mut m = Material::new();
    let image = m.put(&[0, 255, 128, 1], "image/png");
    let mut c = context();
    c.messages[0].content.push(ContentBlock::Image {
        artifact: image,
        detail: None,
        cache: None,
    });
    for profile in [
        EndpointProfile::AnthropicMessages,
        EndpointProfile::OpenAiResponses,
        EndpointProfile::CompatibleChat,
    ] {
        let b = binding(profile.clone());
        let r = request(&mut m, &c);
        let p = prepare(
            identity(b.clone()),
            &r,
            &caps(b.clone()),
            &m,
            None,
            &limits(),
        )
        .unwrap();
        let v = String::from_utf8(p.body).unwrap();
        assert!(v.contains("AP+AAQ=="));
        let mut cap = caps(b.clone());
        cap.images = Support::Unknown;
        assert!(matches!(
            prepare(identity(b), &r, &cap, &m, None, &limits()),
            Err(ProviderError::Unsupported("images"))
        ));
    }
}
#[test]
fn schema_ref_digest_and_limits_are_enforced_before_prepared_bytes() {
    let mut m = Material::new();
    let mut r = request(&mut m, &context());
    let schema = m.schema_ref();
    r.options.output_schema = Some(schema.clone());
    let b = binding(EndpointProfile::OpenAiResponses);
    let cap = caps(b.clone());
    let p = prepare(identity(b.clone()), &r, &cap, &m, None, &limits()).unwrap();
    assert_eq!(body(&p)["text"]["format"]["schema"], m.schema);
    m.schema = json!({"type":"string"});
    assert!(matches!(
        prepare(identity(b.clone()), &r, &cap, &m, None, &limits()),
        Err(ProviderError::InvalidInput("schema_digest"))
    ));
    r.options.output_schema = None;
    let mut small = limits();
    small.request_bytes = 1;
    assert!(matches!(
        prepare(identity(b), &r, &cap, &m, None, &small),
        Err(ProviderError::TooLarge)
    ));
}
#[test]
fn openrouter_auto_beta_uses_its_distinct_retained_plugin_and_provider_constraints() {
    let o:crate::protocol::records::OpenRouterConstraints=serde_json::from_value(json!({"allowedModels":["vendor/*"],"excludedModels":["vendor/old"],"costTier":"low","provider":{"order":["preferred"],"allowFallbacks":false,"requireParameters":true,"dataCollection":"deny","maxPrice":{"prompt":"1.5","completion":"3"}}})).unwrap();
    let map = crate::services::router::connections::open_router_request_fields(
        &o,
        "openrouter/auto-beta",
    );
    assert_eq!(map["plugins"][0]["id"], "auto-beta-router");
    assert_eq!(map["plugins"][0]["allowed_models"], json!(["vendor/*"]));
    assert_eq!(map["provider"]["allow_fallbacks"], false);
    assert_eq!(map["provider"]["max_price"]["prompt"], 1.5);
}
#[test]
fn context_is_versioned_closed_and_cannot_supply_host_authority() {
    let mut v = serde_json::to_value(context()).unwrap();
    v["actorId"] = json!("human");
    assert!(serde_json::from_value::<ProviderContext>(v).is_err());
    let mut c = context();
    c.version = 2;
    assert_eq!(
        c.validate(),
        Err(ProviderError::InvalidInput("context_version"))
    );
}

#[test]
fn tool_result_errors_and_grouped_images_keep_call_and_artifact_order() {
    for profile in [
        EndpointProfile::CompatibleChat,
        EndpointProfile::OpenRouterChat,
        EndpointProfile::OpenAiResponses,
        EndpointProfile::CodexSubscriptionResponses,
    ] {
        let mut m = Material::new();
        let one = m.put(&[1], "image/png");
        let two = m.put(&[2], "image/png");
        let args = m.put(b"{}", "application/json");
        let mut c = context();
        c.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::ToolCall {
                    call_id: "first".into(),
                    wire_name: "tool".into(),
                    arguments: args.clone(),
                },
                ContentBlock::ToolCall {
                    call_id: "second".into(),
                    wire_name: "tool".into(),
                    arguments: args,
                },
            ],
        });
        c.messages.push(Message {
            role: MessageRole::User,
            content: vec![
                ContentBlock::ToolResult {
                    call_id: "first".into(),
                    content: vec![
                        ResultBlock::Text {
                            text: "first failed".into(),
                        },
                        ResultBlock::Image {
                            artifact: one,
                            detail: None,
                        },
                    ],
                    is_error: true,
                    cache: None,
                },
                ContentBlock::ToolResult {
                    call_id: "second".into(),
                    content: vec![
                        ResultBlock::Text {
                            text: "second result".into(),
                        },
                        ResultBlock::Image {
                            artifact: two,
                            detail: None,
                        },
                    ],
                    is_error: false,
                    cache: None,
                },
                ContentBlock::Text {
                    text: "Continue".into(),
                    cache: None,
                },
            ],
        });
        let b = binding(profile.clone());
        let r = request(&mut m, &c);
        let p = prepare(identity(b.clone()), &r, &caps(b), &m, None, &limits()).unwrap();
        let v = body(&p);
        let responses = matches!(
            profile,
            EndpointProfile::OpenAiResponses | EndpointProfile::CodexSubscriptionResponses
        );
        let rows = if responses {
            v["input"].as_array().unwrap()
        } else {
            v["messages"].as_array().unwrap()
        };
        let outputs = rows
            .iter()
            .filter(|r| {
                if responses {
                    r["type"] == "function_call_output"
                } else {
                    r["role"] == "tool"
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(outputs.len(), 2);
        assert_eq!(
            outputs[0][if responses { "call_id" } else { "tool_call_id" }],
            "first"
        );
        assert_eq!(
            outputs[1][if responses { "call_id" } else { "tool_call_id" }],
            "second"
        );
        let error: Value = serde_json::from_str(
            outputs[0][if responses { "output" } else { "content" }]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(error["is_error"], true);
        assert_eq!(error["content"][0]["text"], "first failed");
        let images = rows
            .iter()
            .find(|r| r["role"] == "user" && r["content"].as_array().is_some_and(|c| c.len() == 2))
            .unwrap();
        let urls = images["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                if responses {
                    v["image_url"].as_str().unwrap()
                } else {
                    v["image_url"]["url"].as_str().unwrap()
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            urls,
            ["data:image/png;base64,AQ==", "data:image/png;base64,Ag=="]
        );
        let output_index = rows
            .iter()
            .position(|r| std::ptr::eq(r, outputs[1]))
            .unwrap();
        let image_index = rows.iter().position(|r| std::ptr::eq(r, images)).unwrap();
        assert!(image_index > output_index);
        assert!(
            !String::from_utf8(p.body)
                .unwrap()
                .contains("[image omitted")
        );
    }
}

#[test]
fn current_registered_tool_metadata_is_required_and_descriptions_reach_each_profile() {
    let mut m = Material::new();
    for profile in [
        EndpointProfile::AnthropicMessages,
        EndpointProfile::ClaudeSubscriptionMessages,
        EndpointProfile::OpenAiResponses,
        EndpointProfile::CodexSubscriptionResponses,
        EndpointProfile::CompatibleChat,
        EndpointProfile::OpenRouterChat,
    ] {
        let mut r = request(&mut m, &context());
        let alias = ToolSchema {
            capability_id: "test/lookup".to_owned().try_into().unwrap(),
            wire_name: "lookup".into(),
            input_schema: m.schema_ref(),
        };
        if let InferenceInput::Generation { tools, .. } = &mut r.input {
            tools.push(alias.clone());
        }
        let b = binding(profile.clone());
        let p = prepare(identity(b.clone()), &r, &caps(b), &m, None, &limits()).unwrap();
        let v = body(&p);
        let tool = if matches!(
            profile,
            EndpointProfile::CompatibleChat | EndpointProfile::OpenRouterChat
        ) {
            &v["tools"][0]["function"]
        } else {
            &v["tools"][0]
        };
        assert_eq!(tool["description"], "Look up the exact current record.");
        assert_eq!(tool["name"], "lookup");
        assert_eq!(p.tools[0].capability_id.as_str(), "test/lookup");
    }
    struct Bare;
    impl Materials for Bare {
        fn artifact(&self, _: &ArtifactRef, _: u64) -> Result<Vec<u8>, ProviderError> {
            Err(ProviderError::MaterialUnavailable)
        }
        fn schema(&self, _: &SchemaRef) -> Result<Value, ProviderError> {
            Ok(json!({"type":"object"}))
        }
    }
    let alias = ToolSchema {
        capability_id: "test/lookup".to_owned().try_into().unwrap(),
        wire_name: "lookup".into(),
        input_schema: m.schema_ref(),
    };
    assert!(matches!(
        materialize::tools(&Bare, &[alias], |_| Ok(())),
        Err(ProviderError::MaterialUnavailable)
    ));
}
#[test]
fn fixed_embedding_models_omit_dimensions_and_unknown_support_never_guesses() {
    let mut m = Material::new();
    let text = m.put(b"input", "text/plain");
    let mut r = request(&mut m, &context());
    r.purpose = InferencePurpose::Embedding;
    r.input = InferenceInput::Embedding {
        embedding_space_id: id("space"),
        dimensions: 1536,
        inputs: vec![text],
    };
    let b = binding(EndpointProfile::Embedding);
    let mut c = caps(b.clone());
    c.embedding_dimensions = EmbeddingDimensions::Fixed(1536);
    let p = prepare(identity(b.clone()), &r, &c, &m, None, &limits()).unwrap();
    assert!(body(&p).get("dimensions").is_none());
    assert_eq!(p.embedding_dimensions, Some(1536));
    c.embedding_dimensions = EmbeddingDimensions::Fixed(768);
    assert!(matches!(
        prepare(identity(b.clone()), &r, &c, &m, None, &limits()),
        Err(ProviderError::Unsupported("embedding_dimensions"))
    ));
    c.embedding_dimensions = EmbeddingDimensions::Unknown;
    assert!(matches!(
        prepare(identity(b), &r, &c, &m, None, &limits()),
        Err(ProviderError::Unsupported("embedding_dimensions_unknown"))
    ));
}

#[test]
fn compatible_text_only_content_and_system_prefix_remain_stable_strings() {
    let mut c = context();
    c.instructions.push(TextBlock {
        text: "Second rule".into(),
        cache: None,
    });
    c.messages[0].content.push(ContentBlock::Text {
        text: "Second paragraph".into(),
        cache: None,
    });
    let first = run(EndpointProfile::CompatibleChat, &c);
    let v = body(&first);
    assert_eq!(
        v["messages"][0],
        json!({"role":"system","content":"Rules\n\nSecond rule"})
    );
    assert_eq!(
        v["messages"][1],
        json!({"role":"user","content":"Hello\nSecond paragraph"})
    );
    c.messages.push(Message {
        role: MessageRole::Assistant,
        content: vec![ContentBlock::Text {
            text: "Answer".into(),
            cache: None,
        }],
    });
    c.messages.push(Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: "Next turn".into(),
            cache: None,
        }],
    });
    let next = body(&run(EndpointProfile::CompatibleChat, &c));
    assert_eq!(
        &v["messages"].as_array().unwrap()[..],
        &next["messages"].as_array().unwrap()[..2]
    );
}

#[test]
fn qualified_tool_cache_breakpoints_preserve_order_and_refuse_unknown_controls() {
    let mut m = Material::new();
    let mut c = context();
    c.instructions[0].cache = Some(CacheBreakpoint {
        ttl: Some("5m".into()),
    });
    c.parameters.tool_cache = Some(BTreeMap::from([(
        "lookup".into(),
        CacheBreakpoint {
            ttl: Some("1h".into()),
        },
    )]));
    let mut r = request(&mut m, &c);
    let alias = ToolSchema {
        capability_id: "test/lookup".to_owned().try_into().unwrap(),
        wire_name: "lookup".into(),
        input_schema: m.schema_ref(),
    };
    if let InferenceInput::Generation { tools, .. } = &mut r.input {
        tools.push(alias);
    }
    let b = binding(EndpointProfile::AnthropicMessages);
    let mut cap = caps(b.clone());
    cap.cache_maximum_breakpoints = Some(2);
    let p = prepare(identity(b.clone()), &r, &cap, &m, None, &limits()).unwrap();
    let v = body(&p);
    assert_eq!(v["tools"][0]["name"], "lookup");
    assert_eq!(
        v["tools"][0]["cache_control"],
        json!({"type":"ephemeral","ttl":"1h"})
    );
    assert_eq!(
        v["tools"][0]["description"],
        "Look up the exact current record."
    );
    cap.cache_maximum_breakpoints = Some(1);
    assert!(matches!(
        prepare(identity(b.clone()), &r, &cap, &m, None, &limits()),
        Err(ProviderError::Unsupported("cache_breakpoint_count"))
    ));
    cap.cache_maximum_breakpoints = Some(2);
    cap.cache_tool_breakpoints = Support::Unknown;
    assert!(matches!(
        prepare(identity(b), &r, &cap, &m, None, &limits()),
        Err(ProviderError::Unsupported("tool_cache_breakpoints"))
    ));
}
#[test]
fn qualified_messages_betas_are_merged_only_for_the_selected_access_profile() {
    let mut m = Material::new();
    let mut c = context();
    c.parameters.anthropic_betas = Some(vec!["interleaved-thinking-2025-05-14".into()]);
    for profile in [
        EndpointProfile::AnthropicMessages,
        EndpointProfile::ClaudeSubscriptionMessages,
    ] {
        let b = binding(profile.clone());
        let mut cap = caps(b.clone());
        cap.anthropic_betas = vec!["interleaved-thinking-2025-05-14".into()];
        let r = request(&mut m, &c);
        let p = prepare(identity(b.clone()), &r, &cap, &m, None, &limits()).unwrap();
        let expected = if profile == EndpointProfile::ClaudeSubscriptionMessages {
            "interleaved-thinking-2025-05-14,oauth-2025-04-20"
        } else {
            "interleaved-thinking-2025-05-14"
        };
        assert!(
            p.headers
                .iter()
                .any(|(k, v)| k == "anthropic-beta" && v == expected)
        );
        cap.anthropic_betas.clear();
        assert!(matches!(
            prepare(identity(b), &r, &cap, &m, None, &limits()),
            Err(ProviderError::Unsupported("anthropic_beta"))
        ));
    }
    c.parameters.anthropic_betas = Some(vec!["oauth-2025-04-20".into()]);
    let r = request(&mut m, &c);
    let b = binding(EndpointProfile::AnthropicMessages);
    let mut cap = caps(b.clone());
    cap.anthropic_betas = vec!["oauth-2025-04-20".into()];
    assert!(matches!(
        prepare(identity(b), &r, &cap, &m, None, &limits()),
        Err(ProviderError::Unsupported("anthropic_beta"))
    ));
    let mut c = context();
    c.instructions[0].text =
        "  You are Claude Code, Anthropic's official CLI for Claude. Extra instructions".into();
    assert_eq!(
        body(&run(EndpointProfile::ClaudeSubscriptionMessages, &c))["system"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
#[test]
fn effort_encoding_is_exact_per_profile_and_never_silently_removed() {
    let mut m = Material::new();
    for (profile, encoding, field) in [
        (
            EndpointProfile::CompatibleChat,
            EffortEncoding::ReasoningEffort {
                value: "high".into(),
            },
            "/reasoning_effort",
        ),
        (
            EndpointProfile::OpenRouterChat,
            EffortEncoding::ReasoningEffort {
                value: "high".into(),
            },
            "/reasoning/effort",
        ),
        (
            EndpointProfile::OpenAiResponses,
            EffortEncoding::ResponsesReasoning {
                value: "high".into(),
            },
            "/reasoning/effort",
        ),
        (
            EndpointProfile::AnthropicMessages,
            EffortEncoding::MessagesOutputConfig {
                value: "high".into(),
            },
            "/output_config/effort",
        ),
    ] {
        let b = binding(profile);
        let mut cap = caps(b.clone());
        cap.efforts = vec!["High".into()];
        cap.effort_encodings.insert("High".into(), encoding);
        let mut r = request(&mut m, &context());
        r.options.effort = Some(EffortOption {
            value: id("High"),
            required: true,
        });
        let p = prepare(identity(b.clone()), &r, &cap, &m, None, &limits()).unwrap();
        assert_eq!(body(&p).pointer(field), Some(&json!("high")));
        cap.effort_encodings.clear();
        assert!(matches!(
            prepare(identity(b), &r, &cap, &m, None, &limits()),
            Err(ProviderError::Unsupported("effort"))
        ));
    }
}

#[test]
fn converted_message_and_tool_text_keep_frozen_newlines_without_changing_instructions() {
    for profile in [
        EndpointProfile::CompatibleChat,
        EndpointProfile::OpenRouterChat,
        EndpointProfile::OpenAiResponses,
        EndpointProfile::CodexSubscriptionResponses,
    ] {
        let mut m = Material::new();
        let args = m.put(b"{}", "application/json");
        let mut c = context();
        c.instructions.push(TextBlock {
            text: "Second rule".into(),
            cache: None,
        });
        c.messages[0].content.push(ContentBlock::Text {
            text: "Next paragraph".into(),
            cache: None,
        });
        c.messages.push(Message {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "First assistant".into(),
                    cache: None,
                },
                ContentBlock::Text {
                    text: "Second assistant".into(),
                    cache: None,
                },
                ContentBlock::ToolCall {
                    call_id: "call".into(),
                    wire_name: "lookup".into(),
                    arguments: args,
                },
            ],
        });
        c.messages.push(Message {
            role: MessageRole::User,
            content: vec![ContentBlock::ToolResult {
                call_id: "call".into(),
                content: vec![
                    ResultBlock::Text {
                        text: "First result".into(),
                    },
                    ResultBlock::Text {
                        text: "Second result".into(),
                    },
                ],
                is_error: false,
                cache: None,
            }],
        });
        let b = binding(profile.clone());
        let r = request(&mut m, &c);
        let p = prepare(identity(b.clone()), &r, &caps(b), &m, None, &limits()).unwrap();
        let v = body(&p);
        if matches!(
            profile,
            EndpointProfile::CompatibleChat | EndpointProfile::OpenRouterChat
        ) {
            assert_eq!(v["messages"][0]["content"], "Rules\n\nSecond rule");
            assert_eq!(v["messages"][1]["content"], "Hello\nNext paragraph");
            assert_eq!(
                v["messages"][2]["content"],
                "First assistant\nSecond assistant"
            );
            assert_eq!(v["messages"][3]["content"], "First result\nSecond result");
        } else {
            assert_eq!(v["instructions"], "Rules\n\nSecond rule");
            let result = v["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["type"] == "function_call_output")
                .unwrap();
            assert_eq!(result["output"], "First result\nSecond result");
        }
    }
}

#[test]
fn thinking_effort_thresholds_share_exact_wire_and_admission_normalization() {
    use effort::{NativeEffort, normalize_effort, requested_effort};
    let levels = [
        (0, "low"),
        (4095, "low"),
        (4096, "medium"),
        (16383, "medium"),
        (16384, "high"),
        (32767, "high"),
        (32768, "xhigh"),
        (65535, "xhigh"),
        (65536, "max"),
        (u32::MAX, "max"),
    ];
    for profile in [
        EndpointProfile::OpenAiResponses,
        EndpointProfile::CodexSubscriptionResponses,
        EndpointProfile::CompatibleChat,
        EndpointProfile::OpenRouterChat,
    ] {
        for (budget, level) in levels {
            let mut c = context();
            c.parameters.thinking = Some(Thinking::Enabled {
                budget_tokens: budget,
            });
            let mut m = Material::new();
            let r = request(&mut m, &c);
            let b = binding(profile.clone());
            let mut cap = caps(b.clone());
            cap.efforts = vec![level.into()];
            let encoding = match profile {
                EndpointProfile::OpenAiResponses | EndpointProfile::CodexSubscriptionResponses => {
                    EffortEncoding::ResponsesReasoning {
                        value: level.into(),
                    }
                }
                _ => EffortEncoding::ReasoningEffort {
                    value: level.into(),
                },
            };
            cap.effort_encodings.insert(level.into(), encoding);
            let normalized = normalize_effort(&c.parameters, &r.options, &cap).unwrap();
            assert_eq!(normalized.requested.unwrap().as_str(), level);
            assert_eq!(normalized.requested, normalized.effective);
            let prepared = prepare(identity(b), &r, &cap, &m, None, &limits()).unwrap();
            let pointer = if profile == EndpointProfile::CompatibleChat {
                "/reasoning_effort"
            } else {
                "/reasoning/effort"
            };
            assert_eq!(body(&prepared).pointer(pointer), Some(&json!(level)));
            assert!(body(&prepared).get("thinking").is_none());
        }
    }
    let mut c = context();
    let mut m = Material::new();
    let r = request(&mut m, &c);
    c.parameters.thinking = Some(Thinking::Disabled);
    assert_eq!(
        requested_effort(&c.parameters, &r.options).unwrap(),
        Some(NativeEffort::None)
    );
    c.parameters.thinking = Some(Thinking::Adaptive);
    assert_eq!(requested_effort(&c.parameters, &r.options).unwrap(), None);
    c.parameters.thinking = None;
    assert_eq!(requested_effort(&c.parameters, &r.options).unwrap(), None);
}

#[test]
fn thinking_disabled_override_unknown_and_wrong_encoding_never_downgrade() {
    for profile in [
        EndpointProfile::OpenAiResponses,
        EndpointProfile::CodexSubscriptionResponses,
        EndpointProfile::CompatibleChat,
        EndpointProfile::OpenRouterChat,
    ] {
        let mut c = context();
        c.parameters.thinking = Some(Thinking::Disabled);
        let mut m = Material::new();
        let mut r = request(&mut m, &c);
        let b = binding(profile.clone());
        let mut cap = caps(b.clone());
        let encoding = |value: &str| match profile {
            EndpointProfile::OpenAiResponses | EndpointProfile::CodexSubscriptionResponses => {
                EffortEncoding::ResponsesReasoning {
                    value: value.into(),
                }
            }
            _ => EffortEncoding::ReasoningEffort {
                value: value.into(),
            },
        };
        cap.efforts = vec!["none".into(), "high".into()];
        cap.effort_encodings.insert("none".into(), encoding("none"));
        cap.effort_encodings.insert("high".into(), encoding("high"));
        let pointer = if profile == EndpointProfile::CompatibleChat {
            "/reasoning_effort"
        } else {
            "/reasoning/effort"
        };
        assert_eq!(
            body(&prepare(identity(b.clone()), &r, &cap, &m, None, &limits()).unwrap())
                .pointer(pointer),
            Some(&json!("none"))
        );
        r.options.effort = Some(EffortOption {
            value: id("High"),
            required: true,
        });
        let normalized = effort::normalize_effort(&c.parameters, &r.options, &cap).unwrap();
        assert_eq!(normalized.effective, Some(effort::NativeEffort::High));
        assert_eq!(
            body(&prepare(identity(b.clone()), &r, &cap, &m, None, &limits()).unwrap())
                .pointer(pointer),
            Some(&json!("high"))
        );
        r.options.effort = Some(EffortOption {
            value: id("Minimal"),
            required: false,
        });
        assert_eq!(
            prepare(identity(b.clone()), &r, &cap, &m, None, &limits()).unwrap_err(),
            ProviderError::Unsupported("effort")
        );
        r.options.effort = Some(EffortOption {
            value: id("mystery"),
            required: true,
        });
        assert_eq!(
            effort::requested_effort(&c.parameters, &r.options),
            Err(ProviderError::Unsupported("effort"))
        );
        r.options.effort = None;
        cap.thinking = Support::Unknown;
        assert_eq!(
            prepare(identity(b.clone()), &r, &cap, &m, None, &limits()).unwrap_err(),
            ProviderError::Unsupported("thinking")
        );
        cap.thinking = Support::Supported;
        cap.effort_encodings.insert(
            "none".into(),
            EffortEncoding::MessagesOutputConfig {
                value: "none".into(),
            },
        );
        assert_eq!(
            prepare(identity(b), &r, &cap, &m, None, &limits()).unwrap_err(),
            ProviderError::Unsupported("effort_encoding")
        );
    }
}

#[test]
fn messages_thinking_keeps_exact_native_budget_adaptive_and_disabled() {
    for profile in [
        EndpointProfile::AnthropicMessages,
        EndpointProfile::ClaudeSubscriptionMessages,
    ] {
        for (thinking, expected) in [
            (Thinking::Disabled, json!({"type":"disabled"})),
            (
                Thinking::Enabled {
                    budget_tokens: 16384,
                },
                json!({"type":"enabled","budget_tokens":16384}),
            ),
            (Thinking::Adaptive, json!({"type":"adaptive"})),
        ] {
            let mut c = context();
            c.parameters.thinking = Some(thinking);
            let mut m = Material::new();
            let mut r = request(&mut m, &c);
            let b = binding(profile.clone());
            let mut cap = caps(b.clone());
            let prepared = prepare(identity(b.clone()), &r, &cap, &m, None, &limits()).unwrap();
            assert_eq!(body(&prepared)["thinking"], expected);
            assert!(body(&prepared).get("output_config").is_none());
            cap.efforts = vec!["minimal".into()];
            cap.effort_encodings.insert(
                "minimal".into(),
                EffortEncoding::MessagesOutputConfig {
                    value: "minimal".into(),
                },
            );
            r.options.effort = Some(EffortOption {
                value: id("Minimal"),
                required: true,
            });
            let prepared = prepare(identity(b), &r, &cap, &m, None, &limits()).unwrap();
            assert_eq!(body(&prepared)["thinking"], expected);
            assert_eq!(body(&prepared)["output_config"]["effort"], json!("minimal"));
        }
    }
}

#[test]
fn adaptive_and_unspecified_effort_keep_target_defaults_and_disabled_local_kwargs_are_exact() {
    for profile in [
        EndpointProfile::OpenAiResponses,
        EndpointProfile::CodexSubscriptionResponses,
        EndpointProfile::CompatibleChat,
        EndpointProfile::OpenRouterChat,
    ] {
        let mut c = context();
        c.parameters.thinking = Some(Thinking::Adaptive);
        let mut m = Material::new();
        let r = request(&mut m, &c);
        let b = binding(profile.clone());
        let cap = caps(b.clone());
        let normalized = effort::normalize_effort(&c.parameters, &r.options, &cap).unwrap();
        assert_eq!(normalized.effective, None);
        assert!(normalized.encoding.is_none());
        let prepared = prepare(identity(b), &r, &cap, &m, None, &limits()).unwrap();
        let body = body(&prepared);
        assert!(body.pointer("/reasoning/effort").is_none());
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());
        if profile == EndpointProfile::OpenRouterChat {
            assert_eq!(body["reasoning"], json!({"enabled":true,"exclude":false}));
        }
    }
    let mut c = context();
    c.parameters.thinking = Some(Thinking::Disabled);
    let mut m = Material::new();
    let r = request(&mut m, &c);
    let b = binding(EndpointProfile::CompatibleChat);
    let mut cap = caps(b.clone());
    cap.efforts = vec!["none".into()];
    cap.effort_encodings.insert(
        "none".into(),
        EffortEncoding::ChatTemplateKwargs {
            value: json!({"enable_thinking":false}),
        },
    );
    assert_eq!(
        body(&prepare(identity(b), &r, &cap, &m, None, &limits()).unwrap())["chat_template_kwargs"],
        json!({"enable_thinking":false})
    );
    let embedding = caps(binding(EndpointProfile::Embedding));
    assert!(effort::normalize_effort(&c.parameters, &r.options, &embedding).is_err());
}

#[test]
fn repeated_image_references_stop_expanding_at_the_request_budget() {
    for profile in [
        EndpointProfile::AnthropicMessages,
        EndpointProfile::ClaudeSubscriptionMessages,
        EndpointProfile::OpenAiResponses,
        EndpointProfile::CodexSubscriptionResponses,
        EndpointProfile::CompatibleChat,
        EndpointProfile::OpenRouterChat,
    ] {
        let mut materials = Material::new();
        let image = materials.put(&vec![7; 16 * 1024], "image/png");
        let mut context = context();
        context.messages[0].content = (0..512)
            .map(|_| ContentBlock::Image {
                artifact: image.clone(),
                detail: None,
                cache: None,
            })
            .collect();
        let request = request(&mut materials, &context);
        let binding = binding(profile);
        let limits = PreparationLimits {
            request_bytes: 48 * 1024,
            ..limits()
        };
        let result = prepare(
            identity(binding.clone()),
            &request,
            &caps(binding),
            &materials,
            None,
            &limits,
        );
        assert!(matches!(result, Err(ProviderError::TooLarge)));
        assert!(
            materials.reads.get() <= 4,
            "expanded {} artifacts before rejecting a 48KiB request",
            materials.reads.get()
        );
    }
}

#[test]
fn repeated_native_and_tool_json_artifacts_share_the_request_expansion_budget() {
    for profile in [
        EndpointProfile::AnthropicMessages,
        EndpointProfile::ClaudeSubscriptionMessages,
        EndpointProfile::OpenAiResponses,
        EndpointProfile::CodexSubscriptionResponses,
        EndpointProfile::CompatibleChat,
        EndpointProfile::OpenRouterChat,
    ] {
        for native in [false, true] {
            let mut materials = Material::new();
            let binding = binding(profile.clone());
            let payload = if !native {
                json!({"payload":"x".repeat(16*1024)})
            } else {
                match profile {
                    EndpointProfile::AnthropicMessages
                    | EndpointProfile::ClaudeSubscriptionMessages => {
                        json!({"type":"thinking","thinking":"x".repeat(16*1024),"signature":"signed"})
                    }
                    EndpointProfile::CompatibleChat | EndpointProfile::OpenRouterChat => {
                        json!({"reasoning":"x".repeat(16*1024)})
                    }
                    _ => {
                        json!({"type":"reasoning","id":"native-reasoning","encrypted_content":"x".repeat(16*1024)})
                    }
                }
            };
            let artifact =
                materials.put(&serde_json::to_vec(&payload).unwrap(), "application/json");
            let mut context = context();
            context.messages = (0..512)
                .map(|i| Message {
                    role: MessageRole::Assistant,
                    content: vec![if native {
                        ContentBlock::Native {
                            binding: binding.clone(),
                            artifact: artifact.clone(),
                        }
                    } else {
                        ContentBlock::ToolCall {
                            call_id: format!("call-{i}"),
                            wire_name: "lookup".into(),
                            arguments: artifact.clone(),
                        }
                    }],
                })
                .collect();
            let request = request(&mut materials, &context);
            let result = prepare(
                identity(binding.clone()),
                &request,
                &caps(binding),
                &materials,
                None,
                &PreparationLimits {
                    request_bytes: 48 * 1024,
                    ..limits()
                },
            );
            assert!(
                matches!(result, Err(ProviderError::TooLarge)),
                "{profile:?}/{native}: {result:?}"
            );
            assert!(
                materials.reads.get() <= 5,
                "{profile:?}/{native}: expanded {} artifacts",
                materials.reads.get()
            );
        }
    }
}

#[test]
fn embedding_input_artifacts_also_share_the_request_expansion_budget() {
    let mut materials = Material::new();
    let text = materials.put(&vec![b'a'; 16 * 1024], "text/plain");
    let mut request = request(&mut materials, &context());
    request.purpose = InferencePurpose::Embedding;
    request.input = InferenceInput::Embedding {
        embedding_space_id: id("space"),
        dimensions: 3,
        inputs: vec![text; 64],
    };
    let binding = binding(EndpointProfile::Embedding);
    let result = prepare(
        identity(binding.clone()),
        &request,
        &caps(binding),
        &materials,
        None,
        &PreparationLimits {
            request_bytes: 48 * 1024,
            ..limits()
        },
    );
    assert!(matches!(result, Err(ProviderError::TooLarge)));
    assert!(materials.reads.get() <= 4);
}

#[test]
fn registered_tool_schemas_and_descriptions_stop_materializing_at_the_request_budget() {
    for profile in [
        EndpointProfile::AnthropicMessages,
        EndpointProfile::ClaudeSubscriptionMessages,
        EndpointProfile::OpenAiResponses,
        EndpointProfile::CodexSubscriptionResponses,
        EndpointProfile::CompatibleChat,
        EndpointProfile::OpenRouterChat,
    ] {
        for large_schema in [true, false] {
            let mut materials = Material::new();
            if large_schema {
                materials.schema = json!({"type":"object","description":"s".repeat(16*1024)});
            } else {
                materials.tool_description = "d".repeat(16 * 1024);
            }
            let schema = materials.schema_ref();
            let mut request = request(&mut materials, &context());
            let InferenceInput::Generation { tools, .. } = &mut request.input else {
                unreachable!()
            };
            *tools = (0..256)
                .map(|i| ToolSchema {
                    capability_id: format!("test/tool-{i}").try_into().unwrap(),
                    wire_name: format!("tool_{i}"),
                    input_schema: schema.clone(),
                })
                .collect();
            let binding = binding(profile.clone());
            let limits = PreparationLimits {
                request_bytes: 48 * 1024,
                ..limits()
            };
            let result = prepare(
                identity(binding.clone()),
                &request,
                &caps(binding.clone()),
                &materials,
                None,
                &limits,
            );
            assert!(matches!(result, Err(ProviderError::TooLarge)));
            assert!(
                materials.tool_reads.get() <= 4,
                "{profile:?} large_schema={large_schema}: resolved {} tools before rejecting a48KiB request",
                materials.tool_reads.get()
            );
            let InferenceInput::Generation { tools, .. } = &mut request.input else {
                unreachable!()
            };
            tools.truncate(2);
            materials.tool_reads.set(0);
            let accepted = prepare(
                identity(binding.clone()),
                &request,
                &caps(binding),
                &materials,
                None,
                &limits,
            )
            .unwrap();
            assert_eq!(materials.tool_reads.get(), 2);
            assert_eq!(accepted.tools.len(), 2);
            assert!(accepted.body.len() <= limits.request_bytes);
        }
    }
}
