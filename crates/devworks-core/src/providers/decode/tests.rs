use super::*;
use serde_json::json;

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
