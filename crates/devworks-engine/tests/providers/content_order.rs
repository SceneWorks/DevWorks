use super::*;

#[test]
fn aggregate_native_text_order_is_retained_by_the_actual_session_writer() {
    runtime().block_on(async {
        for (profile, body) in [
            ("AnthropicMessages", json!({"model":"fixture-model","content":[{"type":"thinking","thinking":"first","signature":"actual-signature"},{"type":"text","text":"middle"},{"type":"redacted_thinking","data":"actual-opaque"},{"type":"text","text":"last"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})),
            ("OpenAiResponses", json!({"model":"fixture-model","status":"completed","output":[{"id":"r1","type":"reasoning","encrypted_content":"actual-first","summary":[]},{"id":"m1","type":"message","content":[{"type":"output_text","text":"middle"}]},{"id":"r2","type":"reasoning","encrypted_content":"actual-last","summary":[]},{"id":"m2","type":"message","content":[{"type":"output_text","text":"last"}]}],"usage":{"input_tokens":1,"output_tokens":1}})),
        ] {
            let f = Fx::new(profile, Script::aggregate(&body.to_string()), false);
            let result = f.produce(f.generation(vec![])).await.unwrap();
            assert_eq!(result.result.outcome, Some(AttemptEnd::Completed));
            let order: Vec<_> = f.transcript().into_iter().map(|entry| entry.kind).collect();
            assert_eq!(order, ["NativeItem", "AssistantAttempt", "NativeItem", "AssistantAttempt"], "{profile}");
        }
    });
}

fn body(profile: &str, tools: bool) -> Value {
    if profile == "AnthropicMessages" {
        let mut content = vec![
            json!({"type":"thinking","thinking":"first","signature":"actual-signature"}),
            json!({"type":"text","text":"middle"}),
        ];
        if tools {
            content.push(
                json!({"type":"tool_use","id":"actual-call","name":"list_sessions","input":{}}),
            );
        }
        content.extend([
            json!({"type":"redacted_thinking","data":"actual-opaque"}),
            json!({"type":"text","text":"last"}),
        ]);
        json!({"model":"fixture-model","content":content,"stop_reason":if tools {"tool_use"}else{"end_turn"},"usage":{"input_tokens":1,"output_tokens":1}})
    } else {
        let mut output = vec![
            json!({"id":"r1","type":"reasoning","encrypted_content":"actual-first","summary":[]}),
            json!({"id":"m1","type":"message","content":[{"type":"output_text","text":"middle"}]}),
        ];
        if tools {
            output.push(json!({"id":"item-call","type":"function_call","call_id":"actual-call","name":"list_sessions","arguments":"{}","status":"completed"}));
        }
        output.extend([
            json!({"id":"r2","type":"reasoning","encrypted_content":"actual-last","summary":[]}),
            json!({"id":"m2","type":"message","content":[{"type":"output_text","text":"last"}]}),
        ]);
        json!({"model":"fixture-model","status":"completed","output":output,"usage":{"input_tokens":1,"output_tokens":1}})
    }
}
fn events(profile: &str, body: &Value, end: bool) -> String {
    let mut events = Vec::new();
    if profile == "AnthropicMessages" {
        events.push(json!({"type":"message_start","message":{"id":"message","model":"fixture-model","usage":{"input_tokens":1,"output_tokens":0}}}));
        for (index, block) in body["content"].as_array().unwrap().iter().enumerate() {
            events.push(json!({"type":"content_block_start","index":index,"content_block":block}));
            events.push(json!({"type":"content_block_stop","index":index}));
        }
        if end {
            events.push(json!({"type":"message_delta","delta":{"stop_reason":body["stop_reason"]},"usage":{"output_tokens":1}}));
            events.push(json!({"type":"message_stop"}));
        }
    } else {
        for (index, item) in body["output"].as_array().unwrap().iter().enumerate() {
            events.push(
                json!({"type":"response.output_item.added","output_index":index,"item":item}),
            );
            if item["type"] == "message" {
                for (part, text) in item["content"].as_array().unwrap().iter().enumerate() {
                    events.push(json!({"type":"response.output_text.delta","output_index":index,"content_index":part,"delta":text["text"]}));
                }
            }
            events
                .push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
        }
        if end {
            events.push(json!({"type":"response.completed","response":body}));
        }
    }
    events
        .iter()
        .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
        .collect()
}

#[test]
fn streaming_text_is_visible_while_native_and_calls_wait_for_physical_close() {
    runtime().block_on(async {
        for profile in ["AnthropicMessages", "OpenAiResponses"] {
            let mut script = Script::sse(&events(profile, &body(profile, true), true));
            let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
            script.close_gate = Some(gate.clone());
            let closing = script.closing.clone();
            let f = Arc::new(Fx::new(profile, script, false));
            let tool = f
                .driver
                .schemas()
                .builtin(
                    "sessions.list",
                    devworks_core::registry::Mode::Query,
                    devworks_core::registry::SchemaRevision(1),
                    "list_sessions",
                )
                .unwrap();
            let request = f.generation(vec![tool]);
            let owner = f.clone();
            let producing = tokio::spawn(async move { owner.produce(request).await });
            closing.notified().await;
            let preview = f
                .driver
                .text_preview(&id("session"))
                .expect("actual native text preview before close");
            assert_eq!(preview.text, "middlelast");
            assert!(preview.finalized_through.is_none());
            assert!(
                f.transcript().is_empty(),
                "no provisional native, calls or text rows"
            );
            assert!(!f.http.lock().unwrap().closed.load(Ordering::SeqCst));
            *gate.0.lock().unwrap() = true;
            gate.1.notify_all();
            let result = producing.await.unwrap().unwrap();
            assert_eq!(result.result.outcome, Some(AttemptEnd::ToolCallsReady));
            let rows = f.transcript();
            assert_eq!(
                rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(),
                [
                    "NativeItem",
                    "AssistantAttempt",
                    "ToolCallRecorded",
                    "NativeItem",
                    "AssistantAttempt"
                ],
                "{profile}"
            );
            let finalized = f.driver.text_preview(&id("session")).unwrap();
            assert_eq!(finalized.attempt_id, preview.attempt_id);
            assert_eq!(finalized.finalized_through, rows.last().map(|r| r.sequence));
            assert!(finalized.revision > preview.revision);
        }
    });
}

#[test]
fn finalized_native_artifacts_replay_in_order_into_the_actual_next_request() {
    runtime().block_on(async {
        for profile in ["AnthropicMessages", "OpenAiResponses"] {
            for streaming in [false, true] {
                let response = body(profile, false);
                let script = if streaming {
                    Script::sse(&events(profile, &response, true))
                } else {
                    Script::aggregate(&response.to_string())
                };
                let f = Fx::new(profile, script, false);
                f.produce(f.generation(vec![])).await.unwrap();
                let prior = f.transcript();
                let content = prior
                    .clone()
                    .into_iter()
                    .map(|r| {
                        let body: TranscriptBody = serde_json::from_value(r.body).unwrap();
                        match body {
                            TranscriptBody::AssistantAttempt { visible_text, .. } => {
                                ContentBlock::Text {
                                    text: visible_text,
                                    cache: None,
                                }
                            }
                            TranscriptBody::NativeItem { binding, artifact } => {
                                ContentBlock::Native { binding, artifact }
                            }
                            _ => panic!("unexpected turn body"),
                        }
                    })
                    .collect();
                let context = ProviderContext {
                    version: 1,
                    instructions: vec![],
                    messages: vec![Message {
                        role: MessageRole::Assistant,
                        content,
                    }],
                    parameters: GenerationParameters {
                        response_mode: Some(devworks_core::providers::ResponseMode::Aggregate),
                        ..Default::default()
                    },
                };
                let mut next = f.generation(vec![]);
                next.request_id = id("next-request");
                if let InferenceInput::Generation { context: input, .. } = &mut next.input {
                    *input = f.bytes(&serde_json::to_vec(&context).unwrap());
                }
                {
                    let mut script = f.http.lock().unwrap();
                    script.content_type = "application/json";
                    script.body = VecDeque::from([response.to_string().into_bytes()]);
                }
                f.produce(next).await.unwrap();
                assert_eq!(
                    &f.transcript()[..prior.len()],
                    prior.as_slice(),
                    "committed prior turn is immutable"
                );
                let script = f.http.lock().unwrap();
                let requests = script.requests.lock().unwrap();
                let actual: Value =
                    serde_json::from_slice(requests.last().unwrap().body.as_ref().unwrap())
                        .unwrap();
                if profile == "AnthropicMessages" {
                    let content = &actual["messages"][0]["content"];
                    assert_eq!(
                        content
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|v| v["type"].as_str().unwrap())
                            .collect::<Vec<_>>(),
                        ["thinking", "text", "redacted_thinking", "text"]
                    );
                    assert_eq!(content[0]["signature"], "actual-signature");
                    assert_eq!(content[2]["data"], "actual-opaque");
                    assert_eq!(content[1]["text"], "middle");
                    assert_eq!(content[3]["text"], "last");
                } else {
                    let input = actual["input"].as_array().unwrap();
                    assert_eq!(
                        input
                            .iter()
                            .map(|v| v["type"].as_str().unwrap_or("message"))
                            .collect::<Vec<_>>(),
                        ["reasoning", "message", "reasoning", "message"]
                    );
                    assert_eq!(input[0]["encrypted_content"], "actual-first");
                    assert_eq!(input[2]["encrypted_content"], "actual-last");
                    assert_eq!(input[1]["content"][0]["text"], "middle");
                    assert_eq!(input[3]["content"][0]["text"], "last");
                }
            }
        }
    });
}

#[test]
fn interrupted_stream_keeps_partial_text_without_releasing_native_or_calls() {
    runtime().block_on(async {
        for profile in ["AnthropicMessages", "OpenAiResponses"] {
            let script = Script::sse(&events(profile, &body(profile, true), false));
            let f = Fx::new(profile, script, false);
            let tool = f
                .driver
                .schemas()
                .builtin(
                    "sessions.list",
                    devworks_core::registry::Mode::Query,
                    devworks_core::registry::SchemaRevision(1),
                    "list_sessions",
                )
                .unwrap();
            let result = f.produce(f.generation(vec![tool])).await.unwrap();
            assert_eq!(result.result.outcome, Some(AttemptEnd::UnknownTail));
            let rows = f.transcript();
            assert!(rows.iter().all(|r| r.kind == "AssistantAttempt"));
            assert_eq!(
                rows.iter()
                    .filter_map(|r| r.body["visibleText"].as_str())
                    .collect::<String>(),
                "middlelast"
            );
            assert!(rows.last().unwrap().body["interrupted"].as_bool().unwrap());
        }
    });
}

#[test]
fn native_text_preview_streams_before_terminal_and_cancelled_close_keeps_only_text() {
    runtime().block_on(async {
        for profile in ["AnthropicMessages", "OpenAiResponses"] {
            let mut script = Script::sse(&events(profile, &body(profile, true), false));
            script.wait_for_cancel = true;
            let waiting = script.waiting.clone();
            let f = Arc::new(Fx::new(profile, script, false));
            let tool = f
                .driver
                .schemas()
                .builtin(
                    "sessions.list",
                    devworks_core::registry::Mode::Query,
                    devworks_core::registry::SchemaRevision(1),
                    "list_sessions",
                )
                .unwrap();
            let request = f.generation(vec![tool]);
            let owner = f.clone();
            let producing = tokio::spawn(async move { owner.produce(request).await });
            waiting.notified().await;
            assert_eq!(
                f.driver.text_preview(&id("session")).unwrap().text,
                "middlelast"
            );
            assert!(f.transcript().is_empty());
            f.usage.stop();
            let result = producing.await.unwrap().unwrap();
            assert_eq!(result.result.outcome, Some(AttemptEnd::UnknownTail));
            let rows = f.transcript();
            assert!(rows.iter().all(|r| r.kind == "AssistantAttempt"));
            assert_eq!(
                rows.iter()
                    .filter_map(|r| r.body["visibleText"].as_str())
                    .collect::<String>(),
                "middlelast"
            );
            assert!(rows.last().unwrap().body["interrupted"].as_bool().unwrap());
        }
    });
}

struct UnconfirmedClose(Arc<ScriptHttp>);
struct UnclosedStream(Box<dyn HttpStream>);
impl StreamingHttp for UnconfirmedClose {
    fn open(
        &self,
        request: &HttpRequest,
        limits: &StreamLimits,
        cancellation: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        Ok(Box::new(UnclosedStream(self.0.open(
            request,
            limits,
            cancellation,
        )?)))
    }
}
impl HttpStream for UnclosedStream {
    fn head(&self) -> &ResponseHead {
        self.0.head()
    }
    fn closer(&self) -> Arc<dyn StreamCloser> {
        self.0.closer()
    }
    fn read(
        &mut self,
        out: &mut [u8],
        cancellation: &Cancellation,
    ) -> Result<usize, TransportError> {
        self.0.read(out, cancellation)
    }
    fn close(&mut self) -> Result<(), TransportError> {
        Err(TransportError {
            phase: SendPhase::AfterSend,
            kind: TransportFailure::Protocol,
        })
    }
}
#[test]
fn unconfirmed_physical_close_releases_no_native_calls_or_canonical_turn() {
    runtime().block_on(async {
        for profile in ["AnthropicMessages", "OpenAiResponses"] {
            let mut f = Fx::new(
                profile,
                Script::sse(&events(profile, &body(profile, true), true)),
                false,
            );
            f.driver = NativeProviders::new(
                f.store.clone(),
                f.artifacts.clone(),
                f.connections.clone(),
                f.usage.clone(),
                f.driver.schemas().clone(),
                ProviderComposition::new(Arc::new(UnconfirmedClose(f.http.clone()))),
            );
            let tool = f
                .driver
                .schemas()
                .builtin(
                    "sessions.list",
                    devworks_core::registry::Mode::Query,
                    devworks_core::registry::SchemaRevision(1),
                    "list_sessions",
                )
                .unwrap();
            assert!(f.produce(f.generation(vec![tool])).await.is_err());
            let attempts = read(f.store.as_ref(), |s| {
                s.usage()
                    .inference_attempts(&UsageAttemptFilter::default(), None, 10)
            });
            assert_eq!(attempts.len(), 1);
            let balance = read(f.store.as_ref(), |s| {
                s.usage().attempt_balance(&attempts[0].attempt_id)
            })
            .unwrap();
            assert!(!balance.local_quiescent);
            assert!(balance.unknown_tail);
            assert!(f.transcript().is_empty());
            assert!(
                f.driver.text_preview(&id("session")).is_none(),
                "abandoned owned preview is cleared"
            );
        }
    });
}

#[test]
fn aggregate_tools_keep_their_position_and_failed_native_turns_release_only_text() {
    runtime().block_on(async {
        for profile in ["AnthropicMessages", "OpenAiResponses"] {
            for failed in [false, true] {
                let mut response = body(profile, true);
                if failed {
                    if profile == "AnthropicMessages" {
                        response["stop_reason"] = json!("refusal");
                    } else {
                        response["status"] = json!("failed");
                    }
                }
                let f = Fx::new(profile, Script::aggregate(&response.to_string()), false);
                let tool = f
                    .driver
                    .schemas()
                    .builtin(
                        "sessions.list",
                        devworks_core::registry::Mode::Query,
                        devworks_core::registry::SchemaRevision(1),
                        "list_sessions",
                    )
                    .unwrap();
                let result = f.produce(f.generation(vec![tool])).await.unwrap();
                let rows = f.transcript();
                if failed {
                    assert_eq!(result.result.outcome, Some(AttemptEnd::Failed));
                    assert!(rows.iter().all(|r| r.kind == "AssistantAttempt"));
                    assert_eq!(
                        rows.iter()
                            .filter_map(|r| r.body["visibleText"].as_str())
                            .collect::<String>(),
                        "middlelast"
                    );
                    assert!(rows.last().unwrap().body["interrupted"].as_bool().unwrap());
                } else {
                    assert_eq!(result.result.outcome, Some(AttemptEnd::ToolCallsReady));
                    assert_eq!(
                        rows.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(),
                        [
                            "NativeItem",
                            "AssistantAttempt",
                            "ToolCallRecorded",
                            "NativeItem",
                            "AssistantAttempt"
                        ]
                    );
                }
            }
        }
    });
}

#[test]
fn response_content_parts_remain_distinct_across_the_same_output_item() {
    runtime().block_on(async {
        for streaming in [false,true] {
            let response=json!({"model":"fixture-model","status":"completed","output":[{"id":"message","type":"message","content":[{"type":"output_text","text":"first part"},{"type":"output_text","text":"second part"}]}]});
            let script=if streaming {Script::sse(&events("OpenAiResponses",&response,true))} else {Script::aggregate(&response.to_string())};
            let f=Fx::new("OpenAiResponses",script,false);
            assert_eq!(f.produce(f.generation(vec![])).await.unwrap().result.outcome,Some(AttemptEnd::Completed));
            let rows=f.transcript();
            assert_eq!(rows.iter().filter_map(|r|r.body["visibleText"].as_str()).collect::<Vec<_>>(),["first part","second part"]);
        }
    });
}

#[test]
fn shutdown_fence_hides_preview_while_source_is_still_unclosed() {
    runtime().block_on(async {
        let profile = "AnthropicMessages";
        let mut script = Script::sse(&events(profile, &body(profile, true), true));
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        script.close_gate = Some(gate.clone());
        let closing = script.closing.clone();
        let f = Arc::new(Fx::new(profile, script, false));
        let tool = f
            .driver
            .schemas()
            .builtin(
                "sessions.list",
                devworks_core::registry::Mode::Query,
                devworks_core::registry::SchemaRevision(1),
                "list_sessions",
            )
            .unwrap();
        let request = f.generation(vec![tool]);
        let owner = f.clone();
        let producing = tokio::spawn(async move { owner.produce(request).await });
        closing.notified().await;
        assert_eq!(
            f.driver.text_preview(&id("session")).unwrap().text,
            "middlelast"
        );
        let drained = f
            .usage
            .drain_until(tokio::time::Instant::now() + std::time::Duration::from_millis(5))
            .await
            .unwrap();
        assert!(!drained.unclosed.is_empty());
        assert!(
            f.driver.text_preview(&id("session")).is_none(),
            "fenced before sink drop or physical close"
        );
        let before = read(f.store.as_ref(), |s| Ok(s.stamp().committed_sequence));
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        let _ = producing.await.unwrap();
        assert!(f.driver.text_preview(&id("session")).is_none());
        assert!(f.transcript().is_empty());
        assert_eq!(
            read(f.store.as_ref(), |s| Ok(s.stamp().committed_sequence)),
            before
        );
    });
}

#[test]
fn failed_canonical_append_and_changed_epoch_hide_owned_preview() {
    runtime().block_on(async {
        for change_epoch in [false,true] {
            let mut script=Script::sse(&events("OpenAiResponses",&body("OpenAiResponses",false),true));
            let gate=Arc::new((Mutex::new(false),std::sync::Condvar::new()));script.close_gate=Some(gate.clone());let closing=script.closing.clone();
            let f=Arc::new(Fx::new("OpenAiResponses",script,false));
            let request=f.generation(vec![]);let owner=f.clone();let producing=tokio::spawn(async move{owner.produce(request).await});
            closing.notified().await;assert!(f.driver.text_preview(&id("session")).is_some());
            if change_epoch {
                write(f.store.as_ref(),|tx|{let mut session=tx.sessions().session(&id("session"))?.unwrap();session.context_epoch+=1;session.revision+=1;tx.sessions().update_session(&session)}).unwrap();
            } else {
                let authority=devworks_core::authority::HostAuthority{store:f.store.as_ref(),clock:f.h.clock.as_ref(),protected_branches:&[]};
                let gateway=devworks_core::gateway::Gateway{authority:&authority,store:f.store.as_ref(),ids:f.h.ids.as_ref()};
                let frame=json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":1,"requestKey":"close-session","body":{"kind":"sessions.end","sessionId":"session","expectedRevision":"1"}});
                let cases=UseCases::new().with_sessions(Arc::new(devworks_core::services::sessions::SessionService::new(f.h.ids.clone(),f.store.clone())));
                let reply=cases.command(&gateway,&f.context,"sessions.end",&serde_json::to_vec(&frame).unwrap()).unwrap();
                assert!(matches!(reply,devworks_core::protocol::CommandReply::Recorded{receipt,..} if matches!(receipt.decision,devworks_core::protocol::results::DomainDecision::Applied{..})));
            }
            assert!(f.driver.text_preview(&id("session")).is_none());
            *gate.0.lock().unwrap()=true;gate.1.notify_all();assert!(producing.await.unwrap().is_err());
            assert!(f.driver.text_preview(&id("session")).is_none());assert!(f.transcript().is_empty());
        }
    });
}

#[test]
fn chat_native_unit_and_tool_group_reserve_their_first_observed_positions() {
    runtime().block_on(async {
        for profile in ["CompatibleChat","OpenRouterChat"] {
            let frames=[
                json!({"choices":[{"index":0,"delta":{"content":"before"},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{"reasoning_content":"actual native unit"},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{"content":"between"},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"actual-call","type":"function","function":{"name":"list_sessions","arguments":"{}"}}]},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{"content":"after"},"finish_reason":"tool_calls"}]}),
            ];
            let mut wire: String=frames.iter().map(|v|format!("data: {v}\n\n")).collect();wire.push_str("data: [DONE]\n\n");
            let f=Fx::new(profile,Script::sse(&wire),false);
            let tool=f.driver.schemas().builtin("sessions.list",devworks_core::registry::Mode::Query,devworks_core::registry::SchemaRevision(1),"list_sessions").unwrap();
            assert_eq!(f.produce(f.generation(vec![tool])).await.unwrap().result.outcome,Some(AttemptEnd::ToolCallsReady));
            let rows=f.transcript();
            assert_eq!(rows.iter().map(|r|r.kind.as_str()).collect::<Vec<_>>(),["AssistantAttempt","NativeItem","AssistantAttempt","ToolCallRecorded","AssistantAttempt"]);
            assert_eq!(rows.iter().filter_map(|r|r.body["visibleText"].as_str()).collect::<Vec<_>>(),["before","between","after"]);
            let native:ArtifactRef=serde_json::from_value(rows[1].body["artifact"].clone()).unwrap();
            let (bytes,_)=f.artifacts.read_exact(f.store.as_ref(),&f.context,&native,65536).unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap()["reasoning_content"],"actual native unit");
            assert_eq!(rows[3].body["toolCall"]["call"]["nativeCallId"],"actual-call");
        }
    });
}

#[test]
fn sqlite_failure_rolls_back_the_entire_canonical_turn_and_clears_the_preview() {
    runtime().block_on(async {
        let mut script=Script::sse(&events("AnthropicMessages",&body("AnthropicMessages",true),true));
        let gate=Arc::new((Mutex::new(false),std::sync::Condvar::new()));script.close_gate=Some(gate.clone());let closing=script.closing.clone();
        let f=Arc::new(Fx::new("AnthropicMessages",script,false));
        let tool=f.driver.schemas().builtin("sessions.list",devworks_core::registry::Mode::Query,devworks_core::registry::SchemaRevision(1),"list_sessions").unwrap();
        let request=f.generation(vec![tool]);let owner=f.clone();let producing=tokio::spawn(async move{owner.produce(request).await});
        closing.notified().await;assert!(f.driver.text_preview(&id("session")).is_some());
        let db=rusqlite::Connection::open(f.h.root.join("workspace.sqlite")).unwrap();
        // Fail a later entry, after the batch already inserted its leading native/text rows.
        db.execute_batch("CREATE TRIGGER fail_canonical BEFORE INSERT ON transcript_chunks WHEN NEW.kind='ToolCallRecorded' BEGIN SELECT RAISE(ABORT,'fixture canonical write failure'); END;").unwrap();
        *gate.0.lock().unwrap()=true;gate.1.notify_all();assert!(producing.await.unwrap().is_err());
        assert!(f.http.lock().unwrap().closed.load(Ordering::SeqCst));
        assert!(f.transcript().is_empty(),"no accepted prefix of the failed canonical batch");
        assert_eq!(read(f.store.as_ref(),|s|s.sessions().session(&id("session"))).unwrap().next_sequence,1);
        assert!(read(f.store.as_ref(),|s|s.sessions().pending_calls(&id("session"),10)).is_empty());
        assert!(f.driver.text_preview(&id("session")).is_none());
    });
}
