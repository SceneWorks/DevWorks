use super::*;
use serde_json::json;

impl Decoder {
    pub(super) fn messages_event(
        &mut self,
        event: Option<&str>,
        v: &Value,
        raw: &str,
        out: &mut Vec<DecodedFrame>,
    ) {
        let kind = v
            .get("type")
            .and_then(Value::as_str)
            .or(event)
            .unwrap_or("");
        if self.ended {
            self.error(DecodeErrorKind::Protocol, "event_after_message_stop", out);
            return;
        }
        match kind {
            "ping" => out.push(DecodedFrame::Heartbeat),
            "message_start" => {
                self.usage(raw, &["message", "usage"], out);
                self.native(None, kind, v, raw, true, out);
            }
            "content_block_start" => {
                let (Some(i), Some(block)) = (index(v, "index"), v.get("content_block")) else {
                    self.error(
                        DecodeErrorKind::InvalidContent,
                        "missing_content_block",
                        out,
                    );
                    return;
                };
                if self.blocks.contains_key(&i) {
                    self.error(DecodeErrorKind::Protocol, "duplicate_content_block", out);
                    return;
                }
                self.native(Some(i), kind, v, raw, true, out);
                let block_raw = usage::raw_at(raw, &["content_block"]).unwrap_or(raw);
                self.message_block(i, block, false, block_raw, out);
            }
            "content_block_delta" => {
                let (Some(i), Some(delta)) = (index(v, "index"), v.get("delta")) else {
                    self.error(DecodeErrorKind::InvalidContent, "missing_block_delta", out);
                    return;
                };
                self.native(Some(i), kind, v, raw, true, out);
                if self.stopped_blocks.contains(&i) {
                    self.error(DecodeErrorKind::Protocol, "delta_after_block_stop", out);
                    return;
                }
                let Some(block) = self.blocks.get_mut(&i) else {
                    self.error(DecodeErrorKind::Protocol, "delta_without_block", out);
                    return;
                };
                let field = match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => "text",
                    Some("thinking_delta") => "thinking",
                    Some("signature_delta") => "signature",
                    Some("input_json_delta") => "partial_json",
                    Some("citations_delta") => {
                        if let Some(citation) = delta.get("citation") {
                            if block.get("citations").is_none() {
                                block["citations"] = json!([]);
                            }
                            if let Some(a) = block["citations"].as_array_mut() {
                                a.push(citation.clone());
                            }
                            return;
                        }
                        self.error(DecodeErrorKind::InvalidContent, "missing_citation", out);
                        return;
                    }
                    _ => {
                        self.error(
                            DecodeErrorKind::UnsupportedReplay,
                            "unknown_messages_delta",
                            out,
                        );
                        return;
                    }
                };
                let Some(text) = delta.get(field).and_then(Value::as_str) else {
                    self.error(DecodeErrorKind::InvalidContent, "invalid_block_delta", out);
                    return;
                };
                if field == "partial_json" {
                    if let Some(call) = self.call(i, out) {
                        if !call.arguments_fragmented {
                            if serde_json::from_str::<Value>(&call.arguments)
                                .ok()
                                .is_some_and(|v| v.as_object().is_some_and(|o| o.is_empty()))
                            {
                                call.arguments.clear();
                            }
                            call.arguments_fragmented = true;
                        }
                        call.arguments.push_str(text);
                    }
                } else {
                    let old = block.get(field).and_then(Value::as_str).unwrap_or("");
                    block[field] = json!(format!("{old}{text}"));
                    if field == "text" {
                        out.push(DecodedFrame::TextDelta {
                            block_index: i,
                            content_index: 0,
                            text: text.into(),
                        });
                    }
                }
            }
            "content_block_stop" => {
                let Some(i) = index(v, "index") else {
                    self.error(DecodeErrorKind::InvalidContent, "missing_block_index", out);
                    return;
                };
                if !self.blocks.contains_key(&i) {
                    self.error(DecodeErrorKind::Protocol, "stop_without_block", out);
                    return;
                }
                if !self.stopped_blocks.insert(i) {
                    self.error(DecodeErrorKind::Protocol, "duplicate_block_stop", out);
                    return;
                }
                if let Some(call) = self.calls.get_mut(&i) {
                    call.complete = true;
                    if let Ok(input) = json_value(&call.arguments) {
                        call.item["input"] = input.clone();
                        self.blocks.get_mut(&i).unwrap()["input"] = input;
                    }
                }
                let block = self.blocks[&i].clone();
                self.native(
                    Some(i),
                    "content_block",
                    &block,
                    &block.to_string(),
                    true,
                    out,
                );
                if matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("thinking" | "redacted_thinking")
                ) {
                    self.continuation(Some(i), "messages_reasoning", &block, out);
                }
            }
            "message_delta" => {
                self.usage(raw, &["usage"], out);
                self.native(None, kind, v, raw, true, out);
                if let Some(reason) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.message_stop(reason, out);
                }
            }
            "message_stop" => {
                if self.blocks.keys().any(|i| !self.stopped_blocks.contains(i)) {
                    self.error(DecodeErrorKind::Protocol, "unclosed_content_block", out);
                }
                self.ended = true;
                self.native(None, kind, v, raw, true, out);
            }
            "error" => {
                self.usage(raw, &["usage"], out);
                self.native(None, kind, v, raw, true, out);
                self.provider_error(v.get("error").unwrap_or(v), out);
                self.ended = true;
            }
            _ => self.native(index(v, "index"), kind, v, raw, false, out),
        }
    }
    fn message_stop(&mut self, reason: &str, out: &mut Vec<DecodedFrame>) {
        let outcome = match reason {
            "end_turn" | "stop_sequence" | "tool_use" => DecodedOutcome::Completed,
            "max_tokens" | "model_context_window_exceeded" | "pause_turn" => {
                DecodedOutcome::Incomplete
            }
            "refusal" => {
                self.error(DecodeErrorKind::Refusal, reason, out);
                DecodedOutcome::Failed
            }
            _ => {
                self.error(DecodeErrorKind::Protocol, "unknown_stop_reason", out);
                DecodedOutcome::OutcomeUnknown
            }
        };
        self.stop = Some((outcome, Some(reason.into())));
    }
    fn message_block(
        &mut self,
        i: u64,
        block: &Value,
        complete: bool,
        raw: &str,
        out: &mut Vec<DecodedFrame>,
    ) {
        let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
        let supported = matches!(kind, "text" | "tool_use" | "thinking" | "redacted_thinking");
        self.native(Some(i), kind, block, raw, supported, out);
        if self.blocks.len() >= self.limits.native_items {
            self.error(DecodeErrorKind::LimitExceeded, "content_blocks", out);
            return;
        }
        self.blocks.insert(i, block.clone());
        if complete && matches!(kind, "thinking" | "redacted_thinking") {
            self.continuation(Some(i), "messages_reasoning", block, out);
        }
        match kind {
            "text" => match block.get("text").and_then(Value::as_str) {
                Some(text) => {
                    if !text.is_empty() {
                        out.push(DecodedFrame::TextDelta {
                            block_index: i,
                            content_index: 0,
                            text: text.into(),
                        });
                    }
                }
                None => self.error(DecodeErrorKind::InvalidContent, "invalid_text_block", out),
            },
            "tool_use" => {
                if let Some(call) = self.call(i, out) {
                    call.id = string(block, "id");
                    call.name = string(block, "name");
                    call.item = block.clone();
                    call.arguments = usage::raw_at(raw, &["input"]).unwrap_or_default().into();
                    call.complete = complete;
                }
            }
            "thinking" if block.get("thinking").is_none_or(|v| !v.is_string()) => {
                self.error(DecodeErrorKind::InvalidContent, "invalid_thinking", out)
            }
            "redacted_thinking" if block.get("data").is_none_or(|v| !v.is_string()) => self.error(
                DecodeErrorKind::InvalidContent,
                "invalid_redacted_thinking",
                out,
            ),
            _ => {}
        }
    }
    pub(super) fn messages_body(&mut self, v: &Value, raw: &str, out: &mut Vec<DecodedFrame>) {
        self.usage(raw, &["usage"], out);
        if let Some(error) = v.get("error") {
            self.provider_error(error, out);
            self.ended = true;
            return;
        }
        self.native(None, "message", v, raw, true, out);
        if let Some(content) = v.get("content").and_then(Value::as_array) {
            for (i, block) in content.iter().enumerate() {
                let ordinal = i.to_string();
                let block_raw = usage::raw_at(raw, &["content", &ordinal]).unwrap_or(raw);
                self.message_block(i as u64, block, true, block_raw, out);
            }
        } else {
            self.error(
                DecodeErrorKind::InvalidContent,
                "missing_message_content",
                out,
            );
        }
        if let Some(reason) = v.get("stop_reason").and_then(Value::as_str) {
            self.message_stop(reason, out);
        }
        self.ended = true;
    }
}
