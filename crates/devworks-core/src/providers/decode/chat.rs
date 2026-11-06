use super::*;

impl Decoder {
    pub(super) fn chat_event(
        &mut self,
        v: &Value,
        raw: &str,
        aggregate: bool,
        out: &mut Vec<DecodedFrame>,
    ) {
        // OpenRouter repeats finish_reason in its final accounting chunk. A finish reason is
        // provisional until [DONE], and every reported usage snapshot remains independently read.
        self.usage(raw, &["usage"], out);
        self.native(None, "chat_completion", v, raw, true, out);
        if let Some(error) = v.get("error").filter(|e| !e.is_null()) {
            self.provider_error(error, out);
            self.ended = true;
            return;
        }
        if self.ended {
            self.error(DecodeErrorKind::Protocol, "chunk_after_done", out);
            return;
        }
        let Some(choices) = v.get("choices").and_then(Value::as_array) else {
            self.error(DecodeErrorKind::InvalidContent, "missing_choices", out);
            return;
        };
        for choice in choices {
            let Some(i) = index(choice, "index") else {
                self.error(DecodeErrorKind::InvalidContent, "missing_choice_index", out);
                continue;
            };
            if i != 0 {
                self.native(Some(i), "additional_chat_choice", choice, raw, false, out);
                continue;
            }
            let body = choice.get(if aggregate { "message" } else { "delta" });
            if let Some(body) = body {
                self.chat_reasoning(body, aggregate, out);
                if let Some(text) = body.get("content").filter(|v| !v.is_null()) {
                    if let Some(text) = text.as_str() {
                        self.chat_text(i, text, out);
                    } else if let Some(parts) = text.as_array() {
                        for part in parts {
                            if part.get("type").and_then(Value::as_str) == Some("text")
                                && part.get("text").is_some_and(Value::is_string)
                            {
                                self.chat_text(i, part["text"].as_str().unwrap(), out);
                            } else {
                                self.native(Some(i), "chat_content", part, raw, false, out);
                            }
                        }
                    } else {
                        self.error(DecodeErrorKind::InvalidContent, "invalid_chat_content", out);
                    }
                }
                if body
                    .get("refusal")
                    .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
                {
                    self.error(DecodeErrorKind::Refusal, "provider_refusal", out);
                }
                if let Some(calls) = body.get("tool_calls").filter(|v| !v.is_null()) {
                    if let Some(calls) = calls.as_array() {
                        for (position, item) in calls.iter().enumerate() {
                            let ordinal = if aggregate {
                                Some(position as u64)
                            } else {
                                index(item, "index")
                            };
                            let Some(ordinal) = ordinal else {
                                self.error(
                                    DecodeErrorKind::InvalidToolCall,
                                    "missing_tool_index",
                                    out,
                                );
                                continue;
                            };
                            if item
                                .get("type")
                                .and_then(Value::as_str)
                                .is_some_and(|t| t != "function")
                            {
                                self.native(
                                    Some(ordinal),
                                    "nonfunction_chat_tool",
                                    item,
                                    raw,
                                    false,
                                    out,
                                );
                                continue;
                            }
                            let mut id_changed = false;
                            if let Some(call) = self.call(ordinal, out) {
                                if let Some(id) = string(item, "id") {
                                    id_changed = call.id.as_ref().is_some_and(|old| old != &id);
                                    call.id = Some(id);
                                }
                                if let Some(f) = item.get("function") {
                                    if let Some(name) = string(f, "name") {
                                        if aggregate {
                                            call.name = Some(name);
                                        } else {
                                            call.name.get_or_insert_default().push_str(&name);
                                        }
                                    }
                                    if let Some(args) = string(f, "arguments") {
                                        if aggregate {
                                            call.arguments = args;
                                        } else {
                                            call.arguments.push_str(&args);
                                        }
                                    }
                                }
                                // Retain each actual fragment in Native; the final item is a
                                // projection of those fragments, never a fabricated call ID.
                                if aggregate {
                                    call.item = item.clone();
                                } else if let Some(fields) = item.as_object() {
                                    if !call.item.is_object() {
                                        call.item = serde_json::json!({});
                                    }
                                    for (field, value) in fields {
                                        if field != "function" && !value.is_null() {
                                            call.item[field] = value.clone();
                                        }
                                    }
                                    if !call.item["function"].is_object() {
                                        call.item["function"] = serde_json::json!({});
                                    }
                                    if let Some(fields) =
                                        item.get("function").and_then(Value::as_object)
                                    {
                                        for (field, value) in fields {
                                            if !matches!(field.as_str(), "name" | "arguments")
                                                && !value.is_null()
                                            {
                                                call.item["function"][field] = value.clone();
                                            }
                                        }
                                    }
                                    if let Some(name) = &call.name {
                                        call.item["function"]["name"] = Value::String(name.clone());
                                    }
                                    call.item["function"]["arguments"] =
                                        Value::String(call.arguments.clone());
                                }
                                call.complete = aggregate;
                            }
                            if id_changed {
                                self.error(
                                    DecodeErrorKind::InvalidToolCall,
                                    "changed_tool_id",
                                    out,
                                );
                            }
                        }
                    } else {
                        self.error(DecodeErrorKind::InvalidToolCall, "invalid_tool_calls", out);
                    }
                }
                if body.get("function_call").is_some_and(|v| !v.is_null()) {
                    // The deprecated form has no call ID. It is retained, never repaired with
                    // a synthetic ID that could authorize a host capability.
                    self.error(
                        DecodeErrorKind::InvalidToolCall,
                        "legacy_function_call_without_native_id",
                        out,
                    );
                }
            } else if !aggregate {
                self.error(DecodeErrorKind::InvalidContent, "missing_delta", out);
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                if self
                    .stop
                    .as_ref()
                    .and_then(|(_, r)| r.as_deref())
                    .is_some_and(|old| old != reason)
                {
                    self.error(DecodeErrorKind::Protocol, "conflicting_finish_reason", out);
                }
                let outcome = match reason {
                    "stop" | "tool_calls" => DecodedOutcome::Completed,
                    "length" => DecodedOutcome::Incomplete,
                    "content_filter" => {
                        self.error(DecodeErrorKind::Refusal, reason, out);
                        DecodedOutcome::Failed
                    }
                    "error" => DecodedOutcome::Failed,
                    _ => {
                        self.error(DecodeErrorKind::Protocol, "unknown_finish_reason", out);
                        DecodedOutcome::OutcomeUnknown
                    }
                };
                if reason == "tool_calls" {
                    for call in self.calls.values_mut() {
                        call.complete = true;
                    }
                }
                self.stop = Some((outcome, Some(reason.into())));
            }
        }
    }
    fn chat_reasoning(&mut self, body: &Value, aggregate: bool, out: &mut Vec<DecodedFrame>) {
        for field in ["reasoning", "reasoning_content"] {
            if let Some(value) = body.get(field).filter(|v| !v.is_null()) {
                let Some(part) = value.as_str() else {
                    self.error(
                        DecodeErrorKind::InvalidContent,
                        "invalid_reasoning_text",
                        out,
                    );
                    continue;
                };
                self.chat_native_slot();
                self.explicit_reasoning |= !part.is_empty();
                if aggregate {
                    self.chat_reasoning[field] = Value::String(part.into());
                } else {
                    let old = self
                        .chat_reasoning
                        .get(field)
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    self.chat_reasoning[field] = Value::String(format!("{old}{part}"));
                }
            }
        }
        let Some(details) = body.get("reasoning_details").filter(|v| !v.is_null()) else {
            return;
        };
        let Some(details) = details.as_array() else {
            self.error(
                DecodeErrorKind::InvalidContent,
                "invalid_reasoning_details",
                out,
            );
            return;
        };
        self.chat_native_slot();
        self.explicit_reasoning |= !details.is_empty();
        if aggregate {
            self.chat_reasoning["reasoning_details"] = Value::Array(details.clone());
            return;
        }
        if self.chat_reasoning.get("reasoning_details").is_none() {
            self.chat_reasoning["reasoning_details"] = serde_json::json!([]);
        }
        for fragment in details {
            let Some(fields) = fragment.as_object() else {
                self.error(
                    DecodeErrorKind::InvalidContent,
                    "invalid_reasoning_detail",
                    out,
                );
                continue;
            };
            let collected = self.chat_reasoning["reasoning_details"]
                .as_array_mut()
                .unwrap();
            let existing = collected.iter_mut().find(|entry| {
                let identity = fragment
                    .get("index")
                    .filter(|v| !v.is_null())
                    .map(|v| entry.get("index") == Some(v))
                    .unwrap_or_else(|| {
                        fragment
                            .get("id")
                            .filter(|v| !v.is_null())
                            .is_some_and(|v| entry.get("id") == Some(v))
                    });
                identity
                    && ["type", "id"].iter().all(|k| {
                        fragment.get(k).filter(|v| !v.is_null()).is_none_or(|v| {
                            entry.get(k).filter(|e| !e.is_null()).is_none_or(|e| e == v)
                        })
                    })
            });
            if let Some(entry) = existing {
                for (name, value) in fields {
                    if matches!(name.as_str(), "text" | "summary" | "data" | "signature")
                        && let (Some(old), Some(part)) =
                            (entry.get(name).and_then(Value::as_str), value.as_str())
                    {
                        entry[name] = Value::String(format!("{old}{part}"));
                        continue;
                    }
                    if !value.is_null() {
                        entry[name] = value.clone();
                    }
                }
            } else {
                collected.push(fragment.clone());
            }
        }
    }
    fn chat_text(&mut self, index: u64, text: &str, out: &mut Vec<DecodedFrame>) {
        let (thinking, text) = self.think_tags.feed(text);
        self.tagged_reasoning.push_str(&thinking);
        if !thinking.is_empty() {
            self.chat_native_slot();
        }
        if !text.is_empty() {
            out.push(DecodedFrame::TextDelta {
                block_index: index,
                content_index: self.chat_text_slot(),
                text,
            });
        }
    }
}
