use super::*;

impl Decoder {
    pub(super) fn responses_event(
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
            self.error(
                DecodeErrorKind::Protocol,
                "event_after_response_terminal",
                out,
            );
            return;
        }
        let ordinal = index(v, "output_index");
        match kind {
            "response.created" | "response.in_progress" | "response.queued" => {
                self.usage(raw, &["response", "usage"], out);
                self.native(ordinal, kind, v, raw, true, out);
            }
            "response.output_text.delta" => {
                self.native(ordinal, kind, v, raw, true, out);
                match (
                    ordinal,
                    index(v, "content_index"),
                    v.get("delta").and_then(Value::as_str),
                ) {
                    (Some(i), Some(c), Some(text)) => {
                        self.seen_text.insert((i, c));
                        out.push(DecodedFrame::TextDelta {
                            block_index: i,
                            content_index: c,
                            text: text.into(),
                        });
                    }
                    _ => self.error(
                        DecodeErrorKind::InvalidContent,
                        "invalid_output_text_delta",
                        out,
                    ),
                }
            }
            "response.refusal.delta" | "response.refusal.done" => {
                self.native(ordinal, kind, v, raw, true, out);
                self.error(DecodeErrorKind::Refusal, "provider_refusal", out);
            }
            "response.output_item.added" | "response.output_item.done" => {
                if let (Some(i), Some(item)) = (ordinal, v.get("item")) {
                    self.response_item(
                        i,
                        item,
                        kind.ends_with(".done"),
                        kind.ends_with(".done"),
                        raw,
                        out,
                    );
                } else {
                    self.error(DecodeErrorKind::InvalidContent, "missing_output_item", out);
                }
            }
            "response.function_call_arguments.delta" | "response.function_call_arguments.done" => {
                self.native(ordinal, kind, v, raw, true, out);
                let Some(i) = ordinal else {
                    self.error(
                        DecodeErrorKind::InvalidToolCall,
                        "missing_output_index",
                        out,
                    );
                    return;
                };
                let done = kind.ends_with(".done");
                let args = v
                    .get(if done { "arguments" } else { "delta" })
                    .and_then(Value::as_str);
                let Some(args) = args else {
                    self.error(
                        DecodeErrorKind::InvalidToolCall,
                        "invalid_arguments_delta",
                        out,
                    );
                    return;
                };
                if let Some(call) = self.call(i, out) {
                    if let Some(id) = string(v, "item_id") {
                        call.item_id = Some(id);
                    }
                    if done {
                        call.arguments = args.into();
                    } else {
                        call.arguments.push_str(args);
                    }
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                self.usage(raw, &["response", "usage"], out);
                self.native(None, kind, v, raw, true, out);
                if let Some(body) = v.get("response") {
                    self.fold_response(body, raw, kind == "response.completed", out);
                    self.response_status(body, Some(kind), out);
                } else {
                    self.error(
                        DecodeErrorKind::InvalidContent,
                        "missing_terminal_response",
                        out,
                    );
                }
                self.ended = true;
            }
            "error" => {
                self.usage(raw, &["usage"], out);
                self.native(None, kind, v, raw, true, out);
                self.provider_error(v.get("error").unwrap_or(v), out);
                self.ended = true;
            }
            // Native reasoning and annotations are retained in order, including signatures and
            // encrypted items. They are not reclassified as user-visible assistant prose.
            "response.reasoning_summary_text.delta"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_text.delta"
            | "response.reasoning_text.done"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.output_text.annotation.added" => {
                self.native(ordinal, kind, v, raw, true, out)
            }
            _ => self.native(ordinal, kind, v, raw, false, out),
        }
    }

    fn response_item(
        &mut self,
        i: u64,
        item: &Value,
        complete: bool,
        observe_text: bool,
        raw: &str,
        out: &mut Vec<DecodedFrame>,
    ) {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        let supported = matches!(
            kind,
            "message" | "function_call" | "reasoning" | "compaction"
        );
        self.native(Some(i), kind, item, raw, supported, out);
        if complete && matches!(kind, "reasoning" | "compaction") {
            self.continuation(Some(i), kind, item, out);
        }
        match kind {
            "function_call" => {
                if let Some(call) = self.call(i, out) {
                    if let Some(id) = string(item, "call_id") {
                        call.id = Some(id);
                    }
                    if let Some(id) = string(item, "id") {
                        call.item_id = Some(id);
                    }
                    if let Some(name) = string(item, "name") {
                        call.name = Some(name);
                    }
                    if let Some(args) = string(item, "arguments")
                        && (complete || call.arguments.is_empty())
                    {
                        call.arguments = args;
                    }
                    call.item = item.clone();
                    call.complete = complete
                        && item
                            .get("status")
                            .and_then(Value::as_str)
                            .is_none_or(|s| s == "completed");
                }
            }
            "message" if observe_text => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for (c, block) in content.iter().enumerate() {
                        match block.get("type").and_then(Value::as_str) {
                            Some("output_text") => {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    if self.seen_text.insert((i, c as u64)) {
                                        out.push(DecodedFrame::TextDelta {
                                            block_index: i,
                                            content_index: c as u64,
                                            text: text.into(),
                                        });
                                    }
                                } else {
                                    self.error(
                                        DecodeErrorKind::InvalidContent,
                                        "invalid_output_text",
                                        out,
                                    );
                                }
                            }
                            Some("refusal") => {
                                self.error(DecodeErrorKind::Refusal, "provider_refusal", out)
                            }
                            _ => self.native(
                                Some(i),
                                "unknown_response_content",
                                block,
                                raw,
                                false,
                                out,
                            ),
                        }
                    }
                } else {
                    self.error(
                        DecodeErrorKind::InvalidContent,
                        "missing_message_content",
                        out,
                    );
                }
            }
            _ => {}
        }
    }
    fn fold_response(&mut self, v: &Value, raw: &str, complete: bool, out: &mut Vec<DecodedFrame>) {
        if let Some(output) = v.get("output").and_then(Value::as_array) {
            for (i, item) in output.iter().enumerate() {
                self.response_item(i as u64, item, complete, true, raw, out);
            }
        } else {
            self.error(
                DecodeErrorKind::InvalidContent,
                "missing_response_output",
                out,
            );
        }
    }
    fn response_status(&mut self, v: &Value, event: Option<&str>, out: &mut Vec<DecodedFrame>) {
        if let Some(error) = v.get("error").filter(|e| !e.is_null()) {
            self.provider_error(error, out);
            return;
        }
        let status = v.get("status").and_then(Value::as_str);
        let outcome = match (status, event) {
            (Some("completed"), _) | (None, Some("response.completed")) => {
                DecodedOutcome::Completed
            }
            (Some("incomplete"), _) | (None, Some("response.incomplete")) => {
                DecodedOutcome::Incomplete
            }
            (Some("cancelled"), _) => DecodedOutcome::Cancelled,
            (Some("failed"), _) | (None, Some("response.failed")) => DecodedOutcome::Failed,
            _ => DecodedOutcome::OutcomeUnknown,
        };
        self.stop = Some((
            outcome,
            v.pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| status.map(str::to_owned)),
        ));
    }
    pub(super) fn responses_body(&mut self, v: &Value, raw: &str, out: &mut Vec<DecodedFrame>) {
        self.usage(raw, &["usage"], out);
        self.native(None, "response", v, raw, true, out);
        self.fold_response(
            v,
            raw,
            v.get("status").and_then(Value::as_str) == Some("completed"),
            out,
        );
        self.response_status(v, None, out);
        self.ended = true;
    }
}
