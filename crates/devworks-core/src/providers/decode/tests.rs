use super::*;
use serde_json::json;

// Deterministic native-wire fixtures, not learned execution or live service calls.
// Retained subscription semantics: simple-router a556b3531b577173195bbc3876b556196447721d,
// Codex/ResponsesStreamTranslator.cs and OpenRouter/OpenRouterReasoning.cs.
// API profiles: developers.openai.com/api/reference/resources/{responses,chat,embeddings};
// platform.claude.com/docs/en/build-with-claude/streaming (Messages version 2023-06-01);
// openrouter.ai/docs/api_reference/streaming (repeated finish_reason accounting chunk).

fn event(d: &mut Decoder, value: Value) -> Vec<DecodedFrame> {
    d.consume_data_event(None, &value.to_string())
}
fn outcome(frames: &[DecodedFrame]) -> &DecodedOutcome {
    frames
        .iter()
        .find_map(|f| {
            if let DecodedFrame::Terminal { outcome, .. } = f {
                Some(outcome)
            } else {
                None
            }
        })
        .unwrap()
}
fn calls(frames: &[DecodedFrame]) -> Vec<&DecodedToolCall> {
    frames
        .iter()
        .filter_map(|f| {
            if let DecodedFrame::ToolCallComplete { call } = f {
                Some(call)
            } else {
                None
            }
        })
        .collect()
}
fn usage(frames: &[DecodedFrame]) -> Vec<&DecodedUsage> {
    frames
        .iter()
        .filter_map(|f| {
            if let DecodedFrame::Usage { observation } = f {
                Some(observation)
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn responses_profiles_preserve_native_identity_order_and_release_only_after_success() {
    for profile in [
        EndpointProfile::OpenAiResponses,
        EndpointProfile::CodexSubscriptionResponses,
    ] {
        let mut d = Decoder::new(profile);
        let mut observed = event(
            &mut d,
            json!({"type":"response.created","response":{"id":"resp-native","model":"actual-serving-model"}}),
        );
        observed.extend(event(&mut d,json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc-native","call_id":"call-native","name":"search","arguments":""}})));
        observed.extend(event(&mut d,json!({"type":"response.function_call_arguments.delta","output_index":2,"item_id":"fc-native","delta":"{\"query\":"})));
        observed.extend(event(&mut d,json!({"type":"response.function_call_arguments.delta","output_index":2,"item_id":"fc-native","delta":"\"λ\"}"})));
        let tool = json!({"type":"function_call","id":"fc-native","call_id":"call-native","name":"search","arguments":"{\"query\":\"λ\"}"});
        let reasoning = json!({"type":"reasoning","id":"rs-native","encrypted_content":"opaque-exact","summary":[{"type":"summary_text","text":"reason"}]});
        observed.extend(event(
            &mut d,
            json!({"type":"response.output_item.done","output_index":0,"item":reasoning}),
        ));
        observed.extend(event(
            &mut d,
            json!({"type":"response.output_item.done","output_index":2,"item":tool}),
        ));
        assert!(calls(&observed).is_empty());
        observed.extend(event(&mut d,json!({"type":"response.completed","response":{"id":"resp-native","model":"actual-serving-model","status":"completed","output":[reasoning,{"type":"message","id":"msg-native","content":[{"type":"output_text","text":"hello"}]},tool],"usage":{"input_tokens":20,"output_tokens":4,"input_tokens_details":{"cached_tokens":8},"output_tokens_details":{"reasoning_tokens":3}}}})));
        assert!(calls(&observed).is_empty());
        let final_frames = d.finish();
        assert_eq!(outcome(&final_frames), &DecodedOutcome::ToolCallsReady);
        let call = calls(&final_frames)[0];
        assert_eq!(call.ordinal, 2);
        assert_eq!(call.native_call_id, "call-native");
        assert_eq!(call.native_item_id.as_deref(), Some("fc-native"));
        assert_eq!(call.arguments, json!({"query":"λ"}));
        let continuation = final_frames
            .iter()
            .filter_map(|f| match f {
                DecodedFrame::Native { observation } => observation.continuation.as_ref(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(continuation, vec![&reasoning]);
        assert!(observed.iter().any(|f|matches!(f,DecodedFrame::Identity{model:Some(m),generation_id:Some(id)} if m=="actual-serving-model"&&id=="resp-native")));
        assert_eq!(usage(&observed)[0].reasoning_tokens, Some(3));
        assert!(d.finish().is_empty());
    }
}

#[test]
fn incomplete_failed_and_clean_eof_never_release_observed_calls() {
    for terminal in [None, Some("response.incomplete"), Some("response.failed")] {
        let mut d = Decoder::new(EndpointProfile::CodexSubscriptionResponses);
        let item = json!({"type":"function_call","id":"fc","call_id":"actual","name":"search","arguments":"{}"});
        let mut all = event(
            &mut d,
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
        );
        if let Some(kind) = terminal {
            all.extend(event(&mut d,json!({"type":kind,"response":{"status":if kind.ends_with("failed"){"failed"}else{"incomplete"},"output":[item],"usage":{"output_tokens":7},"incomplete_details":{"reason":"max_output_tokens"}}})));
        }
        all.extend(d.finish());
        assert!(calls(&all).is_empty());
        assert_ne!(outcome(&all), &DecodedOutcome::ToolCallsReady);
        assert!(all.iter().any(|f|matches!(f,DecodedFrame::Native{observation} if observation.value.get("call_id")==Some(&json!("actual")))));
        if terminal.is_some() {
            assert_eq!(usage(&all)[0].output_tokens, Some(7));
        }
    }
}

#[test]
fn messages_profiles_accumulate_json_signatures_redaction_and_disjoint_cache_ttls() {
    for profile in [
        EndpointProfile::AnthropicMessages,
        EndpointProfile::ClaudeSubscriptionMessages,
    ] {
        let mut d = Decoder::new(profile);
        let mut all = event(
            &mut d,
            json!({"type":"message_start","message":{"id":"msg-actual","model":"claude-actual","usage":{"input_tokens":7,"cache_read_input_tokens":11,"cache_creation_input_tokens":5,"cache_creation":{"ephemeral_5m_input_tokens":2,"ephemeral_1h_input_tokens":3}}}}),
        );
        for (i, block) in [
            (0, json!({"type":"thinking","thinking":"","signature":""})),
            (
                1,
                json!({"type":"redacted_thinking","data":"opaque-redacted"}),
            ),
            (
                2,
                json!({"type":"tool_use","id":"tool-native","name":"lookup","input":{}}),
            ),
        ] {
            all.extend(event(
                &mut d,
                json!({"type":"content_block_start","index":i,"content_block":block}),
            ));
            if i == 0 {
                for delta in [
                    json!({"type":"thinking_delta","thinking":"private"}),
                    json!({"type":"signature_delta","signature":"sig-"}),
                    json!({"type":"signature_delta","signature":"exact"}),
                ] {
                    all.extend(event(
                        &mut d,
                        json!({"type":"content_block_delta","index":i,"delta":delta}),
                    ));
                }
            }
            if i == 2 {
                for text in ["{\"x\":", "1}"] {
                    all.extend(event(&mut d,json!({"type":"content_block_delta","index":i,"delta":{"type":"input_json_delta","partial_json":text}})));
                }
            }
            all.extend(event(
                &mut d,
                json!({"type":"content_block_stop","index":i}),
            ));
        }
        all.extend(event(&mut d,json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}})));
        assert!(calls(&all).is_empty());
        all.extend(event(&mut d, json!({"type":"message_stop"})));
        all.extend(d.finish());
        assert_eq!(outcome(&all), &DecodedOutcome::ToolCallsReady);
        assert_eq!(calls(&all)[0].arguments, json!({"x":1}));
        let native = all
            .iter()
            .filter_map(|f| match f {
                DecodedFrame::Native { observation } => observation.continuation.as_ref(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(native[0]["signature"], "sig-exact");
        assert_eq!(native[1]["data"], "opaque-redacted");
        let u = usage(&all);
        assert_eq!(u[0].input_semantics, Some(InputSemantics::Disjoint));
        assert_eq!(u[0].cached_read_tokens, Some(11));
        assert_eq!(
            u[0].cache_writes,
            vec![
                CacheWrite {
                    tokens: 2,
                    ttl_seconds: Some(300)
                },
                CacheWrite {
                    tokens: 3,
                    ttl_seconds: Some(3600)
                }
            ]
        );
        assert_eq!(u[1].output_tokens, Some(9));
        assert_eq!(u[1].input_tokens, None);
    }
}

#[test]
fn openrouter_trailing_usage_repeated_finish_is_one_terminal_and_exact_money() {
    let mut d = Decoder::new(EndpointProfile::OpenRouterChat);
    let mut all = event(
        &mut d,
        json!({"id":"gen-real","model":"upstream/actual","choices":[{"index":0,"delta":{"tool_calls":[{"index":3,"id":"real-call","type":"function","function":{"name":"lookup","arguments":"{\"a\":"}}]},"finish_reason":null}]}),
    );
    all.extend(event(&mut d,json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":3,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}]})));
    let raw = r#"{"id":"gen-real","model":"upstream/actual","choices":[{"index":0,"delta":{"content":""},"finish_reason":"tool_calls","native_finish_reason":"tool_use"}],"usage":{"prompt_tokens":42,"completion_tokens":9,"cost":1e-6,"cost_details":{"upstream_inference_cost":9007199254740993}}}"#;
    all.extend(d.consume_data_event(None, raw));
    assert!(calls(&all).is_empty());
    assert!(
        !all.iter()
            .any(|f| matches!(f, DecodedFrame::Terminal { .. }))
    );
    all.extend(d.consume_data_event(None, "[DONE]"));
    all.extend(d.finish());
    assert_eq!(outcome(&all), &DecodedOutcome::ToolCallsReady);
    assert_eq!(calls(&all)[0].ordinal, 3);
    let u = usage(&all)[0];
    assert_eq!(u.cost.as_ref().unwrap().amount, "0.000001");
    assert_eq!(u.cost.as_ref().unwrap().raw_lexeme, "1e-6");
    assert_eq!(
        u.other_costs["cost_details.upstream_inference_cost"].amount,
        "9007199254740993"
    );
    assert_eq!(
        all.iter()
            .filter(|f| matches!(f, DecodedFrame::Terminal { .. }))
            .count(),
        1
    );
}

#[test]
fn chat_native_reasoning_merges_identified_fragments_without_markers_or_lost_metadata() {
    let mut d = Decoder::new(EndpointProfile::OpenRouterChat);
    for details in [
        json!([{"index":2,"id":"r","type":"reasoning.encrypted","data":"abc","format":"native-v1"}]),
        json!([{"index":2,"id":"r","type":"reasoning.encrypted","data":"def","signature":"sig"}]),
    ] {
        event(
            &mut d,
            json!({"choices":[{"index":0,"delta":{"reasoning_details":details},"finish_reason":null}]}),
        );
    }
    event(
        &mut d,
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    );
    d.consume_data_event(None, "[DONE]");
    let frames = d.finish();
    let c = frames
        .iter()
        .find_map(|f| match f {
            DecodedFrame::Native { observation } => observation.continuation.as_ref(),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        c["reasoning_details"][0],
        json!({"index":2,"id":"r","type":"reasoning.encrypted","data":"abcdef","format":"native-v1","signature":"sig"})
    );
}

#[test]
fn all_seven_profiles_decode_aggregate_and_do_not_invent_model_or_usage() {
    let bodies = [
        (
            EndpointProfile::OpenAiResponses,
            json!({"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"ok"}]}]}),
        ),
        (
            EndpointProfile::CodexSubscriptionResponses,
            json!({"status":"completed","output":[]}),
        ),
        (
            EndpointProfile::AnthropicMessages,
            json!({"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}),
        ),
        (
            EndpointProfile::ClaudeSubscriptionMessages,
            json!({"content":[],"stop_reason":"end_turn"}),
        ),
        (
            EndpointProfile::CompatibleChat,
            json!({"choices":[{"index":0,"message":{"content":"ok"},"finish_reason":"stop"}]}),
        ),
        (
            EndpointProfile::OpenRouterChat,
            json!({"choices":[{"index":0,"message":{"content":"ok"},"finish_reason":"stop"}]}),
        ),
        (
            EndpointProfile::Embedding,
            json!({"data":[{"index":0,"embedding":[0.5,1.0]}]}),
        ),
    ];
    for (profile, body) in bodies {
        let frames = Decoder::new(profile).consume_aggregate(&serde_json::to_vec(&body).unwrap());
        assert_eq!(outcome(&frames), &DecodedOutcome::Completed, "{frames:?}");
        assert!(usage(&frames).is_empty());
        assert!(
            !frames
                .iter()
                .any(|f| matches!(f, DecodedFrame::Identity { .. }))
        );
    }
}

#[test]
fn bad_vectors_do_not_erase_valid_usage_or_exact_cost_and_base64_is_little_endian() {
    let mut d = Decoder::new(EndpointProfile::Embedding);
    let frames=d.consume_aggregate(br#"{"data":[{"index":0,"embedding":"AAAAPw=="},{"index":1,"embedding":"AACAfw=="}],"usage":{"prompt_tokens":3,"cost":0.00000000000001}}"#);
    assert_eq!(outcome(&frames), &DecodedOutcome::Failed);
    assert!(
        frames
            .iter()
            .any(|f| matches!(f,DecodedFrame::Embedding{index:0,vector} if vector==&vec![0.5]))
    );
    assert_eq!(usage(&frames)[0].input_tokens, Some(3));
    assert_eq!(
        usage(&frames)[0].cost.as_ref().unwrap().amount,
        "0.00000000000001"
    );
}

#[test]
fn invalid_native_ids_arguments_unknown_blocks_and_refusals_are_never_repaired() {
    for (id, args) in [
        ("", "{}"),
        ("actual", "{"),
        ("actual", "[]"),
        ("actual", ""),
    ] {
        let frames=Decoder::new(EndpointProfile::OpenAiResponses).consume_aggregate(&serde_json::to_vec(&json!({"status":"completed","output":[{"type":"function_call","id":"item","call_id":id,"name":"lookup","arguments":args}],"usage":{"input_tokens":5}})).unwrap());
        assert!(calls(&frames).is_empty());
        assert_eq!(outcome(&frames), &DecodedOutcome::Failed);
        assert_eq!(usage(&frames)[0].input_tokens, Some(5));
    }
    let frames=Decoder::new(EndpointProfile::AnthropicMessages).consume_aggregate(br#"{"content":[{"type":"future_native","opaque":{"signature":"retain-exact"}}],"stop_reason":"end_turn","usage":{"output_tokens":8}}"#);
    assert_eq!(outcome(&frames), &DecodedOutcome::Failed);
    assert!(frames.iter().any(|f|matches!(f,DecodedFrame::Native{observation} if observation.replay==NativeReplay::Unsupported&&observation.value["opaque"]["signature"]=="retain-exact")));
    assert_eq!(usage(&frames)[0].output_tokens, Some(8));
}

#[test]
fn errors_remain_typed_and_partial_native_facts_survive() {
    for (code, kind) in [
        ("authentication_error", DecodeErrorKind::Authentication),
        ("permission_error", DecodeErrorKind::Permission),
        ("context_length_exceeded", DecodeErrorKind::ContextLimit),
        ("rate_limit_error", DecodeErrorKind::Cooldown),
        ("usage_limit_reached", DecodeErrorKind::Cooldown),
        ("usage_not_included", DecodeErrorKind::ModelUnavailable),
        ("insufficient_quota", DecodeErrorKind::Cooldown),
        ("context_window_exceeded", DecodeErrorKind::ContextLimit),
        ("overloaded_error", DecodeErrorKind::Overloaded),
        ("model_not_found", DecodeErrorKind::ModelUnavailable),
        ("other", DecodeErrorKind::Provider),
    ] {
        let frames = Decoder::new(EndpointProfile::ClaudeSubscriptionMessages).consume_aggregate(
            &serde_json::to_vec(
                &json!({"error":{"type":code,"message":"reported"},"usage":{"output_tokens":4}}),
            )
            .unwrap(),
        );
        assert!(
            frames
                .iter()
                .any(|f| matches!(f,DecodedFrame::Error{error} if error.kind==kind))
        );
        assert_eq!(usage(&frames)[0].output_tokens, Some(4));
        assert!(calls(&frames).is_empty());
    }
}

#[test]
fn decimal_parser_and_invalid_usage_fields_keep_independent_valid_facts() {
    for (raw, expected) in [
        ("1e-6", "0.000001"),
        ("9007199254740993", "9007199254740993"),
        ("2.50e3", "2500"),
        ("0.000", "0"),
    ] {
        assert_eq!(usage::decimal(raw).as_deref(), Some(expected));
    }
    let frames=Decoder::new(EndpointProfile::CompatibleChat).consume_aggregate(br#"{"choices":[{"index":0,"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":-1,"cost":-3}}"#);
    let u = usage(&frames)[0];
    assert_eq!(u.input_tokens, Some(9));
    assert_eq!(u.output_tokens, None);
    assert_eq!(u.cost, None);
    assert_eq!(u.input_semantics, None);
    assert_eq!(u.validation_errors.len(), 2);
}

#[test]
fn response_limits_and_missing_message_stop_refuse_tool_dispatch() {
    let mut d = Decoder::with_limits(
        EndpointProfile::OpenAiResponses,
        DecodeLimits {
            event_bytes: 4,
            ..DecodeLimits::default()
        },
    );
    let frames = d.consume_data_event(None, "{\"x\":1}");
    assert!(frames.iter().any(
        |f| matches!(f,DecodedFrame::Error{error} if error.kind==DecodeErrorKind::LimitExceeded)
    ));
    assert_eq!(outcome(&d.finish()), &DecodedOutcome::Failed);
    let mut d = Decoder::new(EndpointProfile::AnthropicMessages);
    event(
        &mut d,
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}),
    );
    let frames = d.finish();
    assert_eq!(outcome(&frames), &DecodedOutcome::OutcomeUnknown);
    assert!(calls(&frames).is_empty());
}

#[test]
fn interrupted_framing_after_native_success_cannot_release_tools() {
    for interrupted in [
        DecodedOutcome::Cancelled,
        DecodedOutcome::Incomplete,
        DecodedOutcome::OutcomeUnknown,
    ] {
        let mut d = Decoder::new(EndpointProfile::CodexSubscriptionResponses);
        event(
            &mut d,
            json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","id":"i","call_id":"native","name":"lookup","arguments":"{}"}],"usage":{"output_tokens":8}}}),
        );
        let frames = d.finish_interrupted(interrupted.clone(), "transport_or_framer_interrupted");
        assert_eq!(outcome(&frames), &interrupted);
        assert!(calls(&frames).is_empty());
    }
}

#[test]
fn extreme_exact_cost_and_bad_native_json_retain_bounded_evidence() {
    let mut d = Decoder::new(EndpointProfile::OpenRouterChat);
    let frames=d.consume_aggregate(br#"{"choices":[{"index":0,"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"cost":1e400}}"#);
    let u = usage(&frames)[0];
    assert_eq!(u.input_tokens, Some(2));
    assert_eq!(
        u.cost.as_ref().unwrap().amount,
        format!("1{}", "0".repeat(400))
    );
    assert_eq!(u.cost.as_ref().unwrap().raw_lexeme, "1e400");
    let raw = "{\"invalid_arguments\":";
    let mut d = Decoder::new(EndpointProfile::CompatibleChat);
    let frames = d.consume_data_event(None, raw);
    assert!(
        frames
            .iter()
            .any(|f| matches!(f,DecodedFrame::Native{observation} if observation.raw_json==raw))
    );
    assert_eq!(outcome(&d.finish()), &DecodedOutcome::Failed);
}

#[test]
fn duplicate_native_call_ids_and_incomplete_native_items_never_dispatch() {
    let call = json!({"type":"function_call","id":"item","call_id":"same-native","name":"lookup","arguments":"{}"});
    let frames = Decoder::new(EndpointProfile::OpenAiResponses).consume_aggregate(
        &serde_json::to_vec(&json!({"status":"completed","output":[call,call]})).unwrap(),
    );
    assert_eq!(outcome(&frames), &DecodedOutcome::Failed);
    assert!(calls(&frames).is_empty());
    let frames=Decoder::new(EndpointProfile::OpenAiResponses).consume_aggregate(br#"{"status":"completed","output":[{"type":"function_call","status":"incomplete","id":"i","call_id":"c","name":"lookup","arguments":"{}"}]}"#);
    assert_eq!(outcome(&frames), &DecodedOutcome::Failed);
    assert!(calls(&frames).is_empty());
}

#[test]
fn requested_embedding_dimensions_and_duplicate_indices_are_validated_with_usage_retained() {
    let frames=Decoder::new(EndpointProfile::Embedding).with_embedding_dimensions(Some(3))
        .consume_aggregate(br#"{"data":[{"index":0,"embedding":[1,2]},{"index":0,"embedding":[3,4]}],"usage":{"prompt_tokens":7}}"#);
    assert_eq!(outcome(&frames), &DecodedOutcome::Failed);
    assert_eq!(usage(&frames)[0].input_tokens, Some(7));
    assert!(
        !frames
            .iter()
            .any(|f| matches!(f, DecodedFrame::Embedding { .. }))
    );
}

#[test]
fn compatible_chat_native_tool_fragments_and_missing_done_are_not_assumed_complete() {
    for done in [true, false] {
        let mut d = Decoder::new(EndpointProfile::CompatibleChat);
        event(
            &mut d,
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"actual","type":"function","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}),
        );
        if done {
            d.consume_data_event(None, "[DONE]");
        }
        let frames = d.finish();
        assert_eq!(calls(&frames).len(), usize::from(done));
        assert_eq!(
            outcome(&frames),
            &if done {
                DecodedOutcome::ToolCallsReady
            } else {
                DecodedOutcome::OutcomeUnknown
            }
        );
    }
    let frames=Decoder::new(EndpointProfile::CompatibleChat).consume_aggregate(br#"{"choices":[{"index":0,"message":{"refusal":"unsafe"},"finish_reason":"stop"}],"usage":{"completion_tokens":2}}"#);
    assert_eq!(outcome(&frames), &DecodedOutcome::Failed);
    assert_eq!(usage(&frames)[0].output_tokens, Some(2));
    assert!(
        frames.iter().any(
            |f| matches!(f,DecodedFrame::Error{error} if error.kind==DecodeErrorKind::Refusal)
        )
    );
}

#[test]
fn messages_arguments_keep_exact_native_numeric_lexemes_and_empty_fragments_are_not_repaired() {
    let raw=br#"{"content":[{"type":"tool_use","id":"native","name":"lookup","input":{"n":9007199254740993,"small":1e-6}}],"stop_reason":"tool_use"}"#;
    let frames = Decoder::new(EndpointProfile::AnthropicMessages).consume_aggregate(raw);
    assert_eq!(
        calls(&frames)[0].arguments_raw,
        r#"{"n":9007199254740993,"small":1e-6}"#
    );
    let mut d = Decoder::new(EndpointProfile::ClaudeSubscriptionMessages);
    event(
        &mut d,
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"native","name":"lookup","input":{}}}),
    );
    event(
        &mut d,
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":""}}),
    );
    event(&mut d, json!({"type":"content_block_stop","index":0}));
    event(
        &mut d,
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}),
    );
    event(&mut d, json!({"type":"message_stop"}));
    let frames = d.finish();
    assert_eq!(outcome(&frames), &DecodedOutcome::Failed);
    assert!(calls(&frames).is_empty());
}

#[test]
fn messages_escaped_structural_keys_preserve_exact_block_and_argument_bytes() {
    let arguments = r#"{ "n":9007199254740993, "small":1.2300e+004, "escaped":"\u0061" }"#;
    let block =
        format!(r#"{{"type":"tool_use","id":"native","name":"lookup","in\u0070ut":{arguments}}}"#);
    let wire = format!(r#"{{"co\u006etent":[{block}],"stop_reason":"tool_use"}}"#);
    for profile in [
        EndpointProfile::AnthropicMessages,
        EndpointProfile::ClaudeSubscriptionMessages,
    ] {
        let frames = Decoder::new(profile).consume_aggregate(wire.as_bytes());
        assert_eq!(outcome(&frames), &DecodedOutcome::ToolCallsReady);
        assert_eq!(calls(&frames)[0].arguments_raw, arguments);
        assert!(frames.iter().any(|f| matches!(f, DecodedFrame::Native { observation } if observation.kind == "tool_use" && observation.raw_json == block)));
        assert_eq!(frames.iter().filter(|f| matches!(f, DecodedFrame::Native { observation } if observation.raw_json == wire)).count(), 1);
    }
}

#[test]
fn contradictory_native_stop_reasons_are_not_reclassified_as_tool_ready() {
    for reason in ["end_turn", "max_tokens"] {
        let frames=Decoder::new(EndpointProfile::AnthropicMessages).consume_aggregate(&serde_json::to_vec(&json!({"content":[{"type":"tool_use","id":"native","name":"lookup","input":{}}],"stop_reason":reason})).unwrap());
        assert!(calls(&frames).is_empty());
        assert_ne!(outcome(&frames), &DecodedOutcome::ToolCallsReady);
    }
    let frames=Decoder::new(EndpointProfile::CompatibleChat).consume_aggregate(br#"{"choices":[{"index":0,"message":{"tool_calls":[{"id":"native","type":"function","function":{"name":"lookup","arguments":"{}"}}]},"finish_reason":"stop"}]}"#);
    assert!(calls(&frames).is_empty());
    assert_eq!(outcome(&frames), &DecodedOutcome::Failed);
}

#[test]
fn unknown_opaque_reasoning_and_unsigned_thinking_remain_evidence_not_replay() {
    let frames = Decoder::new(EndpointProfile::AnthropicMessages).consume_aggregate(
        br#"{"content":[{"type":"thinking","thinking":"unsigned"}],"stop_reason":"end_turn"}"#,
    );
    assert_eq!(outcome(&frames), &DecodedOutcome::Failed);
    assert!(!frames.iter().any(
        |f| matches!(f,DecodedFrame::Native{observation} if observation.continuation.is_some())
    ));
    let frames=Decoder::new(EndpointProfile::OpenRouterChat).consume_aggregate(br#"{"choices":[{"index":0,"message":{"reasoning_details":[{"type":"reasoning.future","opaque":"exact"}]},"finish_reason":"stop"}]}"#);
    assert_eq!(outcome(&frames), &DecodedOutcome::Failed);
    assert!(frames.iter().any(|f|matches!(f,DecodedFrame::Native{observation} if observation.replay==NativeReplay::Unsupported)));
}

fn text(frames: &[DecodedFrame]) -> String {
    frames
        .iter()
        .filter_map(|f| match f {
            DecodedFrame::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}
fn continuation(frames: &[DecodedFrame]) -> Option<&Value> {
    frames.iter().find_map(|f| match f {
        DecodedFrame::Native { observation } => observation.continuation.as_ref(),
        _ => None,
    })
}

#[test]
fn retained_leading_think_tags_split_at_every_boundary_into_native_reasoning_and_visible_text() {
    let content = " \t\n<think>secret λ</think> \nAnswer <think>mention</think>";
    for profile in [
        EndpointProfile::CompatibleChat,
        EndpointProfile::OpenRouterChat,
    ] {
        let field = if profile == EndpointProfile::CompatibleChat {
            "reasoning_content"
        } else {
            "reasoning"
        };
        let aggregate=Decoder::new(profile.clone()).consume_aggregate(&serde_json::to_vec(&json!({"choices":[{"index":0,"message":{"content":content},"finish_reason":"stop"}]})).unwrap());
        assert_eq!(text(&aggregate), "Answer <think>mention</think>");
        assert_eq!(continuation(&aggregate).unwrap()[field], "secret λ");
        for split in content
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(content.len()))
        {
            let mut d = Decoder::new(profile.clone());
            let mut frames = Vec::new();
            for part in [&content[..split], &content[split..]] {
                frames.extend(event(
                    &mut d,
                    json!({"choices":[{"index":0,"delta":{"content":part},"finish_reason":null}]}),
                ));
            }
            frames.extend(event(
                &mut d,
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            ));
            frames.extend(d.consume_data_event(None, "[DONE]"));
            frames.extend(d.finish());
            assert_eq!(
                text(&frames),
                "Answer <think>mention</think>",
                "split {split}"
            );
            assert_eq!(
                continuation(&frames).unwrap()[field],
                "secret λ",
                "split {split}"
            );
            assert_eq!(outcome(&frames), &DecodedOutcome::Completed);
        }
        let mut d = Decoder::new(profile);
        let mut frames = Vec::new();
        for ch in content.chars() {
            frames.extend(event(&mut d,json!({"choices":[{"index":0,"delta":{"content":ch.to_string()},"finish_reason":null}]})));
        }
        frames.extend(event(
            &mut d,
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        ));
        d.consume_data_event(None, "[DONE]");
        frames.extend(d.finish());
        assert_eq!(text(&frames), "Answer <think>mention</think>");
        assert_eq!(continuation(&frames).unwrap()[field], "secret λ");
    }
}

#[test]
fn retained_think_tag_flush_preserves_nonleading_mentions_partial_tags_and_whitespace() {
    for (content, visible, thinking) in [
        (
            "Text <think>mention</think>",
            "Text <think>mention</think>",
            None,
        ),
        ("  <thi", "  <thi", None),
        (" \t\n", " \t\n", None),
        ("<think>secret</thi", "", Some("secret</thi")),
        ("<think>x</think> \n ", "", Some("x")),
    ] {
        let frames=Decoder::new(EndpointProfile::CompatibleChat).consume_aggregate(&serde_json::to_vec(&json!({"choices":[{"index":0,"message":{"content":content},"finish_reason":"stop"}]})).unwrap());
        assert_eq!(text(&frames), visible);
        assert_eq!(
            continuation(&frames)
                .and_then(|v| v.get("reasoning_content"))
                .and_then(Value::as_str),
            thinking
        );
    }
    // The retained source qualifies only the Chat families; other profiles preserve literal text.
    let frames=Decoder::new(EndpointProfile::AnthropicMessages).consume_aggregate(br#"{"content":[{"type":"text","text":"<think>literal</think>"}],"stop_reason":"end_turn"}"#);
    assert_eq!(text(&frames), "<think>literal</think>");
    assert!(continuation(&frames).is_none());
}

#[test]
fn explicit_chat_reasoning_and_openrouter_details_suppress_duplicate_tagged_reasoning() {
    for profile in [
        EndpointProfile::CompatibleChat,
        EndpointProfile::OpenRouterChat,
    ] {
        for field in ["reasoning_content", "reasoning"] {
            let mut body = json!({"content":"<think>same</think> visible"});
            body[field] = json!("same");
            let frames = Decoder::new(profile.clone()).consume_aggregate(
                &serde_json::to_vec(
                    &json!({"choices":[{"index":0,"message":body,"finish_reason":"stop"}]}),
                )
                .unwrap(),
            );
            assert_eq!(text(&frames), "visible");
            assert_eq!(continuation(&frames).unwrap(), &json!({field:"same"}));
            for explicit_first in [true, false] {
                let mut d = Decoder::new(profile.clone());
                let mut frames = Vec::new();
                let explicit = json!({field:"same"});
                let tagged = json!({"content":"<think>same</think> visible"});
                let deltas = if explicit_first {
                    [explicit, tagged]
                } else {
                    [tagged, explicit]
                };
                for delta in deltas {
                    frames.extend(event(
                        &mut d,
                        json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
                    ));
                }
                event(
                    &mut d,
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                );
                d.consume_data_event(None, "[DONE]");
                frames.extend(d.finish());
                assert_eq!(text(&frames), "visible");
                assert_eq!(continuation(&frames).unwrap(), &json!({field:"same"}));
            }
        }
    }
    let details = json!([{"type":"reasoning.encrypted","id":"native-r","data":"exact-encrypted"}]);
    let frames=Decoder::new(EndpointProfile::OpenRouterChat).consume_aggregate(&serde_json::to_vec(&json!({"choices":[{"index":0,"message":{"content":"<think>duplicate</think> visible","reasoning_details":details},"finish_reason":"stop"}]})).unwrap());
    assert_eq!(text(&frames), "visible");
    assert_eq!(
        continuation(&frames).unwrap(),
        &json!({"reasoning_details":details})
    );
}

#[test]
fn retained_openrouter_null_fragment_identity_can_be_completed_without_duplicate_entries() {
    for initial_type in [Value::Null, json!("reasoning.text")] {
        let mut d = Decoder::new(EndpointProfile::OpenRouterChat);
        for detail in [
            json!({"index":0,"type":initial_type,"id":null,"text":"a"}),
            json!({"index":0,"type":"reasoning.text","id":"r0","text":"b"}),
        ] {
            event(
                &mut d,
                json!({"choices":[{"index":0,"delta":{"reasoning_details":[detail]},"finish_reason":null}]}),
            );
        }
        event(
            &mut d,
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        );
        d.consume_data_event(None, "[DONE]");
        let frames = d.finish();
        assert_eq!(outcome(&frames), &DecodedOutcome::Completed);
        assert_eq!(
            continuation(&frames).unwrap()["reasoning_details"],
            json!([{"index":0,"type":"reasoning.text","id":"r0","text":"ab"}])
        );
    }
}

#[test]
fn responses_item_and_unknown_part_evidence_keeps_exact_native_lexemes() {
    let unknown =
        r#"{ "type":"future_native", "amount":1.2300e+004, "huge":1e9999, "escaped":"\u0061" }"#;
    // Escaped structural keys must resolve to the same field as the parsed convenience view.
    let item = format!(
        r#"{{ "type":"message", "con\u0074ent":[{unknown},{unknown}], "future":9007199254740993 }}"#
    );
    let body = format!(r#"{{"status":"completed","out\u0070ut":[{item}],"opaque":1.2000e-009}}"#);
    for profile in [
        EndpointProfile::OpenAiResponses,
        EndpointProfile::CodexSubscriptionResponses,
    ] {
        for form in 0..3 {
            let mut decoder = Decoder::new(profile.clone());
            let wire = match form {
                0 => body.clone(),
                1 => format!(
                    r#"{{"type":"response.completed","response":{body},"opaque-event":1.2300e+04}}"#
                ),
                _ => format!(
                    r#"{{"type":"response.output_item.done","output_index":3,"item":{item},"opaque-event":1.2300e+04}}"#
                ),
            };
            let frames = if form == 0 {
                decoder.consume_aggregate(wire.as_bytes())
            } else {
                decoder.consume_data_event(None, &wire)
            };
            let observations: Vec<_> = frames
                .iter()
                .filter_map(|frame| match frame {
                    DecodedFrame::Native { observation } => Some(observation),
                    _ => None,
                })
                .collect();
            assert_eq!(
                observations.iter().filter(|o| o.raw_json == wire).count(),
                1,
                "one exact enclosing observation"
            );
            assert_eq!(
                observations
                    .iter()
                    .filter(|o| o.kind == "message"
                        && o.raw_json.as_str()
                            == if form == 2 {
                                wire.as_str()
                            } else {
                                item.as_str()
                            })
                    .count(),
                1
            );
            let parts: Vec<_> = observations
                .iter()
                .filter(|o| o.kind == "unknown_response_content")
                .collect();
            assert_eq!(parts.len(), 2);
            assert!(
                parts.iter().all(
                    |part| part.raw_json == unknown && part.replay == NativeReplay::Unsupported
                )
            );
            assert!(frames.iter().any(|frame|matches!(frame,DecodedFrame::Error{error} if error.kind==DecodeErrorKind::UnsupportedReplay)));
            assert!(calls(&frames).is_empty());
        }
    }
}
