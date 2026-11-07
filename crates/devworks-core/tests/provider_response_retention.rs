//! Bounded native-wire regression for PR40. No transport, inference or external side effects.
use devworks_core::protocol::provider::EndpointProfile;
use devworks_core::providers::decode::{DecodedFrame, DecodedOutcome, Decoder};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static MEASUREMENT: Mutex<()> = Mutex::new(());

struct Allocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static TRACKING: AtomicBool = AtomicBool::new(false);
fn added(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    if TRACKING.load(Ordering::Relaxed) {
        ALLOCATED.fetch_add(bytes, Ordering::Relaxed);
        PEAK.fetch_max(live, Ordering::Relaxed);
    }
}
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            added(layout.size());
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            added(layout.size());
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe {
            System.dealloc(pointer, layout);
        }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = unsafe { System.realloc(pointer, layout, size) };
        if !result.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            added(size);
        }
        result
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

#[test]
fn streamed_chat_reasoning_accumulates_with_linear_allocation_work() {
    let _measurement = MEASUREMENT.lock().unwrap();
    let part = format!("{}λ", "x".repeat(128));
    for profile in [
        EndpointProfile::CompatibleChat,
        EndpointProfile::OpenRouterChat,
    ] {
        for field in ["reasoning", "reasoning_content", "reasoning_details"] {
            let body = if field == "reasoning_details" {
                serde_json::json!({field:[{"index":0,"id":"r","type":"reasoning.encrypted","text":part,"summary":part,"data":part,"signature":part,"format":"exact"}]})
            } else {
                serde_json::json!({field:part})
            };
            let wire =
                serde_json::json!({"choices":[{"index":0,"delta":body,"finish_reason":null}]})
                    .to_string();
            let mut small = None;
            for count in [256, 2_048] {
                let (frames, allocated, _) = measured(|| {
                    let mut decoder = Decoder::new(profile.clone());
                    for _ in 0..count {
                        let chunk = decoder.consume_data_event(None, &wire);
                        assert!(
                            !chunk
                                .iter()
                                .any(|f| matches!(f, DecodedFrame::Error { .. }))
                        );
                        assert_eq!(native(&chunk).filter(|n| n.raw_json == wire).count(), 1);
                    }
                    decoder.consume_data_event(
                        None,
                        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                    );
                    decoder.consume_data_event(None, "[DONE]");
                    decoder.finish()
                });
                assert!(matches!(
                    frames.last(),
                    Some(DecodedFrame::Terminal {
                        outcome: DecodedOutcome::Completed,
                        ..
                    })
                ));
                let continuation = native(&frames)
                    .find_map(|n| n.continuation.as_ref())
                    .unwrap();
                let expected = part.repeat(count);
                if field == "reasoning_details" {
                    let detail = &continuation[field][0];
                    for key in ["text", "summary", "data", "signature"] {
                        assert_eq!(detail[key].as_str(), Some(expected.as_str()));
                    }
                    assert_eq!(detail["id"], "r");
                    assert_eq!(detail["format"], "exact");
                } else {
                    assert_eq!(continuation[field].as_str(), Some(expected.as_str()));
                }
                let wire_bytes = count * wire.len();
                assert!(
                    allocated < wire_bytes * 128,
                    "quadratic prefix copies: {profile:?}/{field}/{count}: {allocated} allocated for {wire_bytes} wire bytes"
                );
                if let Some((bytes, allocations)) = small {
                    assert!(
                        (allocated as u128) * (bytes as u128)
                            < 2 * (allocations as u128) * (wire_bytes as u128),
                        "reasoning allocation work grows faster than input"
                    );
                } else {
                    small = Some((wire_bytes, allocated));
                }
                eprintln!(
                    "reasoning {profile:?}/{field}/{count}: wire={wire_bytes}, allocated={allocated}"
                );
            }
        }
    }
}

fn measured<T>(run: impl FnOnce() -> T) -> (T, usize, usize) {
    struct Stop;
    impl Drop for Stop {
        fn drop(&mut self) {
            TRACKING.store(false, Ordering::SeqCst);
        }
    }
    let before = LIVE.load(Ordering::SeqCst);
    ALLOCATED.store(0, Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    TRACKING.store(true, Ordering::SeqCst);
    let stop = Stop;
    let result = run();
    drop(stop);
    (
        result,
        ALLOCATED.load(Ordering::SeqCst),
        PEAK.load(Ordering::SeqCst).saturating_sub(before),
    )
}

#[test]
fn streamed_chat_tool_arguments_are_assembled_once_at_completion() {
    let _measurement = MEASUREMENT.lock().unwrap();
    let part = format!("{}λ", "x".repeat(128));
    let open = serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":"lookup","arguments":"{\"payload\":\"","format":"native-v1"}}]},"finish_reason":null}]}).to_string();
    let wire = serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":part}}]},"finish_reason":null}]}).to_string();
    let close = serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"}"}}]},"finish_reason":"tool_calls"}]}).to_string();
    for profile in [
        EndpointProfile::CompatibleChat,
        EndpointProfile::OpenRouterChat,
    ] {
        let mut small = None;
        for count in [256, 2_048] {
            let (frames, allocated, _) = measured(|| {
                let mut decoder = Decoder::new(profile.clone());
                decoder.consume_data_event(None, &open);
                for _ in 0..count {
                    let frames = decoder.consume_data_event(None, &wire);
                    assert!(!frames.iter().any(|f| matches!(
                        f,
                        DecodedFrame::Error { .. } | DecodedFrame::ToolCallComplete { .. }
                    )));
                    assert_eq!(native(&frames).filter(|n| n.raw_json == wire).count(), 1);
                }
                decoder.consume_data_event(None, &close);
                decoder.consume_data_event(None, "[DONE]");
                decoder.finish()
            });
            let call = frames
                .iter()
                .find_map(|f| match f {
                    DecodedFrame::ToolCallComplete { call } => Some(call),
                    _ => None,
                })
                .expect("one completed native call");
            let expected = part.repeat(count);
            assert_eq!(call.native_call_id, "call");
            assert_eq!(call.name, "lookup");
            assert_eq!(call.arguments["payload"].as_str(), Some(expected.as_str()));
            assert_eq!(
                call.native_item["function"]["arguments"].as_str(),
                Some(call.arguments_raw.as_str())
            );
            assert_eq!(call.native_item["function"]["name"], "lookup");
            assert_eq!(call.native_item["function"]["format"], "native-v1");
            assert!(matches!(
                frames.last(),
                Some(DecodedFrame::Terminal {
                    outcome: DecodedOutcome::ToolCallsReady,
                    ..
                })
            ));
            let bytes = open.len() + count * wire.len() + close.len();
            assert!(
                allocated < bytes * 128,
                "quadratic tool arguments: {profile:?}/{count}: {allocated} allocated for {bytes} wire bytes"
            );
            if let Some((small_bytes, small_allocated)) = small {
                assert!(
                    (allocated as u128) * (small_bytes as u128)
                        < 2 * (small_allocated as u128) * (bytes as u128)
                );
            } else {
                small = Some((bytes, allocated));
            }
            eprintln!("tool arguments {profile:?}/{count}: wire={bytes}, allocated={allocated}");
        }
    }
}

const ITEM_FIELDS: &str = r#""type":"message", "opaque":{"amount":1.2300e+004,"integer":9007199254740993,"escaped":"\u0061"}, "content":[{"type":"output_text","text":"native text","annotations":[]}]"#;
fn item(index: usize) -> String {
    format!(r#"{{ "id":"msg_{index}", "role":"assistant", "status":"completed", {ITEM_FIELDS} }}"#)
}
fn body(count: usize) -> String {
    let mut raw = String::from(
        r#"{ "id":"native-response", "model":"native-model", "status":"completed", "opaque-envelope":{ "escaped":"\u0062", "amount":9.8700e-009 }, "output":["#,
    );
    for i in 0..count {
        if i != 0 {
            raw.push(',');
        }
        raw.push_str(&item(i));
    }
    raw.push_str("] }");
    raw
}
fn native(
    frames: &[DecodedFrame],
) -> impl Iterator<Item = &devworks_core::providers::decode::NativeObservation> {
    frames.iter().filter_map(|frame| match frame {
        DecodedFrame::Native { observation } => Some(observation),
        _ => None,
    })
}
fn decode(profile: EndpointProfile, raw: &str, streaming: bool) -> Vec<DecodedFrame> {
    let mut decoder = Decoder::new(profile);
    if streaming {
        let mut frames = decoder.consume_data_event(Some("response.completed"), raw);
        assert!(decoder.protocol_ended());
        frames.extend(decoder.finish());
        frames
    } else {
        decoder.consume_aggregate(raw.as_bytes())
    }
}
#[test]
fn aggregate_and_stream_terminal_many_item_retention_and_allocations_are_linear() {
    let _measurement = MEASUREMENT.lock().unwrap();
    for profile in [
        EndpointProfile::OpenAiResponses,
        EndpointProfile::CodexSubscriptionResponses,
    ] {
        for streaming in [false, true] {
            let mut small = None;
            // The 64-item case fails on the old source using <1 MiB, before the large case
            // is ever constructed/decoded. This regression can run RED without an OOM.
            for count in [64, 5_000] {
                let body = body(count);
                let wire = if streaming {
                    format!(
                        r#"{{ "type":"response.completed", "opaque-event":1.23400e+02, "response":{body} }}"#
                    )
                } else {
                    body.clone()
                };
                let (frames, allocated, peak) =
                    measured(|| decode(profile.clone(), &wire, streaming));
                assert!(matches!(
                    frames.last(),
                    Some(DecodedFrame::Terminal {
                        outcome: DecodedOutcome::Completed,
                        ..
                    })
                ));
                let raw_bytes: usize = native(&frames).map(|item| item.raw_json.len()).sum();
                assert!(
                    raw_bytes <= 3 * wire.len(),
                    "quadratic retention: count={count}, wire={}, raw={raw_bytes}",
                    wire.len()
                );
                assert_eq!(
                    native(&frames).filter(|item| item.raw_json == wire).count(),
                    1,
                    "one enclosing native observation"
                );
                let items: Vec<_> = native(&frames)
                    .filter(|item| item.kind == "message")
                    .collect();
                assert_eq!(items.len(), count);
                for (i, item) in items.iter().enumerate() {
                    assert_eq!(item.ordinal, Some(i as u64));
                    assert_eq!(
                        item.raw_json,
                        self::item(i),
                        "exact escapes, decimal lexemes and opaque fields"
                    );
                    assert!(item.continuation.is_none());
                }
                let text: Vec<_> = frames
                    .iter()
                    .filter_map(|frame| match frame {
                        DecodedFrame::TextDelta {
                            block_index,
                            content_index,
                            text,
                        } => Some((*block_index, *content_index, text.as_str())),
                        _ => None,
                    })
                    .collect();
                assert_eq!(text.len(), count);
                assert!(
                    text.iter()
                        .enumerate()
                        .all(|(i, item)| *item == (i as u64, 0, "native text"))
                );
                assert!(
                    allocated < wire.len() * 128,
                    "allocation budget: {allocated} for {} bytes",
                    wire.len()
                );
                assert!(
                    peak < wire.len() * 96,
                    "live allocation budget: {peak} for {} bytes",
                    wire.len()
                );
                eprintln!(
                    "Responses streaming={streaming} count={count} wireBytes={} retainedRaw={raw_bytes} allocated={allocated} peakLive={peak}",
                    wire.len()
                );
                if let Some((small_bytes, small_allocated)) = small {
                    assert!(
                        (allocated as u128) * (small_bytes as u128)
                            < 2 * (small_allocated as u128) * (wire.len() as u128),
                        "allocated bytes grow faster than the input"
                    );
                } else {
                    small = Some((wire.len(), allocated));
                }
            }
        }
    }
}

#[test]
fn messages_many_blocks_parse_work_and_exact_evidence_are_linear() {
    let _measurement = MEASUREMENT.lock().unwrap();
    let block = r#"{ "type":"text", "text":"native text", "opaque":{"amount":1.2300e+004,"integer":9007199254740993,"escaped":"\u0061"} }"#;
    for profile in [
        EndpointProfile::AnthropicMessages,
        EndpointProfile::ClaudeSubscriptionMessages,
    ] {
        let mut small = None;
        for count in [64, 2_000] {
            let wire = format!(
                r#"{{"id":"native-message","type":"message","role":"assistant","content":[{}],"stop_reason":"end_turn"}}"#,
                vec![block; count].join(",")
            );
            let (frames, allocated, peak) = measured(|| decode(profile.clone(), &wire, false));
            assert!(matches!(
                frames.last(),
                Some(DecodedFrame::Terminal {
                    outcome: DecodedOutcome::Completed,
                    ..
                })
            ));
            let blocks: Vec<_> = native(&frames).filter(|item| item.kind == "text").collect();
            assert_eq!(blocks.len(), count);
            for (i, observation) in blocks.iter().enumerate() {
                assert_eq!(observation.ordinal, Some(i as u64));
                assert_eq!(observation.raw_json, block);
            }
            assert_eq!(
                native(&frames).filter(|item| item.raw_json == wire).count(),
                1
            );
            assert_eq!(frames.iter().filter(|f| matches!(f, DecodedFrame::TextDelta { text, .. } if text == "native text")).count(), count);
            eprintln!(
                "Messages count={count} wireBytes={} allocated={allocated} peakLive={peak}",
                wire.len()
            );
            assert!(
                allocated < wire.len() * 128,
                "repeated enclosing-array parse: {allocated} allocated for {} bytes",
                wire.len()
            );
            if let Some((small_bytes, small_allocated)) = small {
                assert!(
                    (allocated as u128) * (small_bytes as u128)
                        < 2 * (small_allocated as u128) * (wire.len() as u128),
                    "parse allocations grow faster than input"
                );
            } else {
                small = Some((wire.len(), allocated));
            }
        }
    }
}

fn chat_decode(profile: EndpointProfile, raw: &str, streaming: bool) -> Vec<DecodedFrame> {
    let mut decoder = Decoder::new(profile);
    if streaming {
        let mut frames = decoder.consume_data_event(None, raw);
        frames.extend(decoder.consume_data_event(None, "[DONE]"));
        frames.extend(decoder.finish());
        frames
    } else {
        decoder.consume_aggregate(raw.as_bytes())
    }
}

#[test]
fn chat_many_choices_retain_one_envelope_and_linear_exact_choice_evidence() {
    let _measurement = MEASUREMENT.lock().unwrap();
    for profile in [
        EndpointProfile::CompatibleChat,
        EndpointProfile::OpenRouterChat,
    ] {
        for streaming in [false, true] {
            let mut small = None;
            for count in [64, 512, 5_000] {
                let field = if streaming { "delta" } else { "message" };
                let choices: Vec<_> = (0..count).map(|i|format!(r#"{{ "index":{i}, "{field}":{{"content":"native text"}}, "finish_reason":"stop", "opaque":1.2300e+004 }}"#)).collect();
                let wire = format!(
                    r#"{{"id":"native-chat","choices":[{}],"opaque":9007199254740993}}"#,
                    choices.join(",")
                );
                let (frames, allocated, peak) =
                    measured(|| chat_decode(profile.clone(), &wire, streaming));
                eprintln!(
                    "Chat choices profile={profile:?} stream={streaming} count={count} wireBytes={} allocated={allocated} peakLive={peak}",
                    wire.len()
                );
                assert!(
                    allocated < wire.len() * 128,
                    "repeated enclosing response copies: {allocated} allocated for {} bytes",
                    wire.len()
                );
                if let Some((small_bytes, small_allocated)) = small {
                    assert!(
                        (allocated as u128) * (small_bytes as u128)
                            < 2 * (small_allocated as u128) * (wire.len() as u128),
                        "choice retention grows faster than input"
                    );
                } else {
                    small = Some((wire.len(), allocated));
                }
                // The bounded RED fails before allocating the old 5,000-choice envelope
                // population. GREEN also covers that full legal input.
                if count == 64 {
                    continue;
                }
                assert!(matches!(
                    frames.last(),
                    Some(DecodedFrame::Terminal {
                        outcome: DecodedOutcome::Failed,
                        ..
                    })
                ));
                let retained: Vec<_> = native(&frames)
                    .filter(|v| v.kind == "additional_chat_choice")
                    .collect();
                assert_eq!(retained.len(), count - 1);
                for (i, observation) in retained.iter().enumerate() {
                    assert_eq!(observation.ordinal, Some((i + 1) as u64));
                    assert_eq!(observation.raw_json, choices[i + 1]);
                    assert_eq!(
                        observation.replay,
                        devworks_core::providers::decode::NativeReplay::Unsupported
                    );
                }
                assert_eq!(
                    native(&frames).filter(|item| item.raw_json == wire).count(),
                    1
                );
                assert_eq!(
                    frames
                        .iter()
                        .filter(
                            |f| matches!(f,DecodedFrame::TextDelta{text,..}if text=="native text")
                        )
                        .count(),
                    1
                );
                assert!(
                    !frames
                        .iter()
                        .any(|f| matches!(f, DecodedFrame::ToolCallComplete { .. }))
                );
            }
        }
    }
}

#[test]
fn chat_nested_unsupported_populations_keep_linear_exact_item_evidence() {
    let _measurement = MEASUREMENT.lock().unwrap();
    for profile in [
        EndpointProfile::CompatibleChat,
        EndpointProfile::OpenRouterChat,
    ] {
        for streaming in [false, true] {
            for tools in [false, true] {
                let mut small = None;
                for count in [64, 512, 2_000] {
                    let items:Vec<_>=(0..count).map(|i| if tools {
                        format!(r#"{{ "index":{i}, "id":"native_{i}", "type":"future_tool", "opaque":9.8700e-009 }}"#)
                    }else{format!(r#"{{ "type":"future_content", "position":{i}, "opaque":1.2300e+004, "escaped":"\u0061" }}"#)}).collect();
                    let field = if streaming { "delta" } else { "message" };
                    let body = if tools {
                        format!(
                            r#"{{"content":"native text","tool_calls":[{}]}}"#,
                            items.join(",")
                        )
                    } else {
                        format!(r#"{{"content":[{}]}}"#, items.join(","))
                    };
                    let wire = format!(
                        r#"{{"choices":[{{"index":0,"{field}":{body},"finish_reason":"stop"}}]}}"#
                    );
                    let (frames, allocated, peak) =
                        measured(|| chat_decode(profile.clone(), &wire, streaming));
                    eprintln!(
                        "Chat nested profile={profile:?} stream={streaming} tools={tools} count={count} wireBytes={} allocated={allocated} peakLive={peak}",
                        wire.len()
                    );
                    assert!(
                        allocated < wire.len() * 128,
                        "nested envelope copies: {allocated} allocated for {} bytes",
                        wire.len()
                    );
                    if let Some((small_bytes, small_allocated)) = small {
                        assert!(
                            (allocated as u128) * (small_bytes as u128)
                                < 2 * (small_allocated as u128) * (wire.len() as u128),
                            "nested retention grows faster than input"
                        );
                    } else {
                        small = Some((wire.len(), allocated));
                    }
                    if count == 64 {
                        continue;
                    }
                    assert!(matches!(
                        frames.last(),
                        Some(DecodedFrame::Terminal {
                            outcome: DecodedOutcome::Failed,
                            ..
                        })
                    ));
                    let kind = if tools {
                        "nonfunction_chat_tool"
                    } else {
                        "chat_content"
                    };
                    let retained: Vec<_> = native(&frames).filter(|v| v.kind == kind).collect();
                    assert_eq!(retained.len(), count);
                    for (i, observation) in retained.iter().enumerate() {
                        assert_eq!(observation.raw_json, items[i]);
                        assert_eq!(observation.ordinal, Some(if tools { i as u64 } else { 0 }));
                        assert_eq!(
                            observation.replay,
                            devworks_core::providers::decode::NativeReplay::Unsupported
                        );
                    }
                    assert_eq!(native(&frames).filter(|v| v.raw_json == wire).count(), 1);
                    assert!(
                        !frames
                            .iter()
                            .any(|f| matches!(f, DecodedFrame::ToolCallComplete { .. }))
                    );
                }
            }
        }
    }
}

#[test]
fn messages_text_thinking_and_signature_append_with_linear_allocation_work() {
    let _measurement = MEASUREMENT.lock().unwrap();
    let part = format!("{}λ", "x".repeat(128));
    for profile in [
        EndpointProfile::AnthropicMessages,
        EndpointProfile::ClaudeSubscriptionMessages,
    ] {
        for field in ["text", "thinking", "signature"] {
            let kind = if field == "text" { "text" } else { "thinking" };
            let block = if field == "text" {
                serde_json::json!({"type":kind,"text":""})
            } else {
                serde_json::json!({"type":kind,"thinking":if field == "thinking" {""} else {"opaque reasoning"},"signature":if field == "signature" {""} else {"signed"}})
            };
            let initial =
                serde_json::json!({"type":"content_block_start","index":0,"content_block":block})
                    .to_string();
            let wire = serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":format!("{field}_delta"),field:part}}).to_string();
            let mut small = None;
            for count in [256, 2048] {
                let (frames, allocated, _) = measured(|| {
                    let mut decoder = Decoder::new(profile.clone());
                    decoder.consume_data_event(None, &initial);
                    for _ in 0..count {
                        let frames = decoder.consume_data_event(None, &wire);
                        assert!(
                            !frames
                                .iter()
                                .any(|f| matches!(f, DecodedFrame::Error { .. }))
                        );
                    }
                    let mut frames = decoder
                        .consume_data_event(None, r#"{"type":"content_block_stop","index":0}"#);
                    frames.extend(decoder.consume_data_event(
                        None,
                        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
                    ));
                    frames.extend(decoder.consume_data_event(None, r#"{"type":"message_stop"}"#));
                    frames.extend(decoder.finish());
                    frames
                });
                let block = native(&frames)
                    .find_map(|item| {
                        serde_json::from_str::<serde_json::Value>(&item.raw_json)
                            .ok()
                            .filter(|value| value.get(field).is_some())
                    })
                    .expect("completed native block");
                assert_eq!(block[field].as_str(), Some(part.repeat(count).as_str()));
                assert!(
                    matches!(
                        frames.last(),
                        Some(DecodedFrame::Terminal {
                            outcome: DecodedOutcome::Completed,
                            ..
                        })
                    ),
                    "{profile:?}/{field}/{count}: {:?}",
                    frames
                        .iter()
                        .filter(|f| matches!(
                            f,
                            DecodedFrame::Error { .. } | DecodedFrame::Terminal { .. }
                        ))
                        .collect::<Vec<_>>()
                );
                let bytes = initial.len() + wire.len() * count;
                assert!(
                    allocated < bytes * 128,
                    "quadratic Messages append: {field}/{count}: {allocated} for {bytes}"
                );
                if let Some((old_bytes, old_allocated)) = small {
                    assert!(
                        (allocated as u128) * (old_bytes as u128)
                            < 2 * (old_allocated as u128) * (bytes as u128),
                        "Messages allocation grows faster than wire input"
                    );
                } else {
                    small = Some((bytes, allocated));
                }
            }
        }
    }
}
