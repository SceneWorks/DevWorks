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
}
impl Material {
    fn new() -> Self {
        Self {
            blobs: BTreeMap::new(),
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
        let b = self
            .blobs
            .get(r.sha256.as_str())
            .ok_or(ProviderError::MaterialUnavailable)?;
        if b.len() as u64 > max {
            return Err(ProviderError::TooLarge);
        }
        Ok(b.clone())
    }
    fn schema(&self, _: &SchemaRef) -> Result<Value, ProviderError> {
        Ok(self.schema.clone())
    }
}
fn context() -> ProviderContext {
    ProviderContext {
        version: 1,
        instructions: vec![TextBlock {
            text: "Rules".into(),
            cache: None,
        }],
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
    c.sampling = Support::Supported;
    c.top_k = Support::Supported;
    c.stop = Support::Supported;
    c.thinking = Support::Supported;
    c.prompt_cache_key = Support::Supported;
    c.prompt_cache_retention = Support::Supported;
    c.cache_breakpoints = Support::Supported;
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
            _ => assert_eq!(v["messages"][1]["content"][0]["text"], "Hello"),
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
