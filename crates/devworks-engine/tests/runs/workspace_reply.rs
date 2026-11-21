use super::*;
use devworks_core::protocol::OperationState;
use devworks_core::services::sessions::workspace_reply::{CAPABILITY, ReplyIntent};

struct WorkspaceEntropy;
impl devworks_core::ports::http::Entropy for WorkspaceEntropy {
    fn fill(&self, bytes: &mut [u8]) {
        bytes.fill(11);
    }
}

fn workspace_ports(f: &Fx, wire: Arc<dyn StreamingHttp>) -> Composition {
    let mut ports = composition(f, None, Arc::new(FakePower::default()));
    ports.application = devworks_engine::ApplicationComposition {
        connections: Some(devworks_engine::ConnectionPorts {
            http: Arc::new(devworks_core::ports::http::NoNetwork),
            secrets: Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
            entropy: Arc::new(WorkspaceEntropy),
            endpoints: None,
        }),
        router: Some(Default::default()),
        sessions: Some(devworks_engine::SessionPorts {
            coordinator: Some(ActorId(id("workspace-coordinator"))),
        }),
        ..Default::default()
    };
    ports.config.providers =
        Some(devworks_engine::providers::driver::ProviderComposition::new(wire));
    ports
}

fn open_home(f: &Fx, app: &devworks_engine::Runtime) -> Id {
    let open = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":1,"requestKey":"native-home",
        "body":{"kind":"sessions.start","scope":{"kind":"Workspace"},"title":"Home"}});
    let session = applied(
        &app.command(&f.user, "sessions.start", open.to_string().as_bytes())
            .unwrap(),
    );
    id(session["id"].as_str().unwrap())
}

#[test]
fn the_native_workspace_coordinator_replies_to_a_saved_home_message() {
    let _native_case = native_runtime_case();
    let f = Fx::new();
    seed_local_route(&f);
    let calls = Arc::new(AtomicUsize::new(0));
    let ports = workspace_ports(&f, Arc::new(AgentWire(calls.clone())));
    runtime().block_on(async {
        let app = boot(ports).await.unwrap();
        let session_id = open_home(&f, &app);
        let command = json!({"protocol":PROTOCOL,"schemaRevision":3,"requestKey":"home-message","sessionId":session_id,
            "expectedSessionRevision":"1","content":[{"kind":"Text","text":"Reply in this Home chat."}],
            "replyRoute":{"intent":{"kind":"Model","modelId":"watch-model"},"effort":"Default"}
        }).to_string();
        applied(&app.command(&f.user, "sessions.append_message", command.as_bytes()).unwrap());
        within("a native workspace reply", async {
            loop {
                let completed = read(&*f.store, |r| r.operations().operations_for_capability(
                    CAPABILITY, &[OperationState::ObservedComplete], None, 10,
                ));
                if let Some(reply) = completed.first() {
                    let attempt = reply.latest_attempt().unwrap();
                    assert!(attempt.observations.iter().any(|v| v["replySettlement"]["kind"] == "WorkspaceReplyCompleted"),
                        "{:?}", attempt.observations);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await;
        let entries = read(&*f.store, |r| r.sessions().entries(&session_id, 0, 30));
        assert!(entries.iter().any(|entry| entry.kind == "AssistantAttempt"
            && entry.body["visibleText"] == "done"), "{entries:?}");
        assert!(replayed(&app.command(&f.user, "sessions.append_message", command.as_bytes()).unwrap()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        app.shutdown().await;
    });
}

#[derive(Default)]
struct WorkspaceWire {
    requests: Mutex<Vec<Value>>,
    release_first: (Mutex<bool>, std::sync::Condvar),
}

impl StreamingHttp for WorkspaceWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let payload: Value = serde_json::from_slice(request.body.as_ref().unwrap()).unwrap();
        let index = {
            let mut requests = self.requests.lock().unwrap();
            let index = requests.len();
            requests.push(payload);
            index
        };
        let (delta, finish) = if index == 0 {
            let (released, signal) = &self.release_first;
            let (_guard, wait) = signal
                .wait_timeout_while(released.lock().unwrap(), Duration::from_secs(20), |v| !*v)
                .unwrap();
            assert!(
                !wait.timed_out(),
                "the next user message should be saved before the tool call"
            );
            (
                json!({"tool_calls":[{"index":0,"id":"current-screen-native","type":"function",
                "function":{"name":"ui_current_screen_query","arguments":"{}"}}]}),
                "tool_calls",
            )
        } else {
            (
                json!({"content":if index == 1 { "First answer" } else { "Second answer" }}),
                "stop",
            )
        };
        let frame = json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
        Ok(Box::new(AgentStream {
            head: ResponseHead {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
            },
            body: Some(format!("data: {frame}\n\ndata: [DONE]\n\n").into_bytes()),
        }))
    }
}

#[test]
fn a_workspace_tool_finishes_before_the_next_saved_message_enters_context() {
    use devworks_core::protocol::records::ScreenContext;
    use devworks_core::protocol::support::BoundedList;
    use devworks_core::services::navigation::NavigationContextService;
    let _native_case = native_runtime_case();
    let f = Fx::new();
    seed_local_route(&f);
    let wire = Arc::new(WorkspaceWire::default());
    let navigation = Arc::new(NavigationContextService::default());
    let mut ports = workspace_ports(&f, wire.clone());
    ports
        .use_cases
        .register("ui.current_screen", Some(navigation.clone()), None);
    runtime().block_on(async {
        let app = boot(ports).await.unwrap();
        let session_id = open_home(&f, &app);
        navigation.publish(
            Some((
                session_id.clone(),
                ScreenContext {
                    screen_id: id("fixture-screen"),
                    project_id: None,
                    root_id: None,
                    selected_ids: BoundedList(vec![]),
                    help_topic_id: id("fixture-help"),
                    revision: Revision::new(1).unwrap(),
                },
            )),
            "fixture-screen",
        );
        let send = |key: &str, revision: &str, text: &str| {
            let frame = json!({"protocol":PROTOCOL,"schemaRevision":3,"requestKey":key,
                "sessionId":session_id,"expectedSessionRevision":revision,
                "content":[{"kind":"Text","text":text}],
                "replyRoute":{"intent":{"kind":"Model","modelId":"watch-model"},"effort":"Default"}
            });
            applied(
                &app.command(
                    &f.user,
                    "sessions.append_message",
                    frame.to_string().as_bytes(),
                )
                .unwrap(),
            );
        };
        send("first-home", "1", "First queued message");
        within("the first workspace request reaches the wire", async {
            while wire.requests.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        send("second-home", "2", "Later queued message");
        *wire.release_first.0.lock().unwrap() = true;
        wire.release_first.1.notify_all();
        within("two workspace replies settle", async {
            loop {
                let completed = read(&*f.store, |r| {
                    r.operations().operations_for_capability(
                        CAPABILITY,
                        &[OperationState::ObservedComplete],
                        None,
                        10,
                    )
                });
                if completed.len() == 2 {
                    assert!(
                        completed.iter().all(|op| op
                            .latest_attempt()
                            .unwrap()
                            .observations
                            .iter()
                            .any(|v| v["replySettlement"]["kind"] == "WorkspaceReplyCompleted")),
                        "{completed:?}"
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        let requests = wire
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>();
        let [first, tool_followup, second] = requests.as_slice() else {
            panic!(
                "expected exactly three provider requests, got {}",
                requests.len()
            );
        };
        assert!(first.contains("ui_current_screen_query"));
        assert!(first.contains("First queued message") && !first.contains("Later queued message"));
        assert!(
            tool_followup.contains("fixture-screen")
                && !tool_followup.contains("Later queued message")
        );
        assert!(second.contains("First answer") && second.contains("Later queued message"));
        app.shutdown().await;
    });
}

#[test]
fn a_legacy_home_send_adopts_the_owner_and_records_one_exact_ordered_reply() {
    let _native_case = native_runtime_case();
    let f = Fx::new();
    let legacy = id("legacy-workspace-session");
    let actor = f.user.principal().clone();
    let session_id = legacy.clone();
    f.tx(move |_, tx| {
        let session = StoredSession {
            id: session_id,
            conversation_id: id("legacy-workspace-conversation"),
            agent_id: actor.clone(),
            project_id: None,
            root_id: None,
            context_epoch: 0,
            revision: 1,
            title: "Existing Home chat".into(),
            created_by: actor,
            created_at_ms: tx.now_ms(),
            closed_at_ms: None,
            closed_by: None,
            next_sequence: 1,
            checkpoint_sequence: 0,
        };
        tx.sessions().insert_session(&session)
    });
    let owner = ActorId(id("workspace-coordinator"));
    let mut ports = composition(&f, None, Arc::new(FakePower::default()));
    ports.application.sessions = Some(devworks_engine::SessionPorts {
        coordinator: Some(owner.clone()),
    });
    runtime().block_on(async {
        let app = boot(ports).await.unwrap();
        let operation = "sessions.append_message";
        let frame = |key, revision| {
            json!({"protocol": "1.0-draft.1", "requestKey": key, "sessionId": legacy,
                "expectedSessionRevision": revision, "content":[{"kind":"Text","text":"First reply"}]})
        };
        let stale = app.command(
            &f.user, operation, frame("stale-legacy", "2").to_string().as_bytes(),
        ).unwrap();
        assert!(matches!(decision(&stale), DomainDecision::Refused { .. }));
        let still_human = read(&*f.store, |r| r.sessions().session(&legacy)).unwrap();
        assert_eq!(&still_human.agent_id, f.user.principal());

        let mut send = frame("first-legacy-reply", "1");
        send["replyRoute"] = json!({"intent":{"kind":"Tier","tier":"Mid"},"effort":"High"});
        send["schemaRevision"] = json!(2);
        assert!(matches!(
            app.command(&f.user, operation, send.to_string().as_bytes()).unwrap(),
            CommandReply::Rejected { .. }
        ));
        send["schemaRevision"] = json!(3);
        let exact = send.to_string();
        applied(&app.command(&f.user, operation, exact.as_bytes()).unwrap());
        assert!(replayed(&app.command(&f.user, operation, exact.as_bytes()).unwrap()));
        send["replyRoute"]["effort"] = json!("Low");
        assert!(matches!(
            app.command(&f.user, operation, send.to_string().as_bytes()).unwrap(),
            CommandReply::Rejected { code: devworks_core::protocol::Rejection::IdempotencyConflict, .. }
        ));
        read(&*f.store, |r| {
            let adopted = r.sessions().session(&legacy)?.unwrap();
            assert_eq!(adopted.agent_id, owner);
            assert_eq!(&adopted.created_by, f.user.principal());
            assert_eq!(adopted.revision, 2);
            let entries = r.sessions().entries(&legacy, 0, 10)?;
            let [entry] = entries.as_slice() else { panic!("one exact saved message") };
            let intents = r.operations().operations_for_capability(
                CAPABILITY, &[OperationState::IntentRecorded], None, 10,
            )?;
            let [intent] = intents.as_slice() else { panic!("one exact reply intent") };
            let intent: ReplyIntent = serde_json::from_value(intent.payload.clone()).unwrap();
            assert_eq!(intent.user_entry_id, entry.entry_id);
            assert_eq!(intent.user_sequence, entry.sequence);
            assert_eq!(intent.session_id, legacy);
            assert_eq!(intent.agent_id, owner);
            assert_eq!(
                serde_json::to_value(intent.route.unwrap()).unwrap(),
                json!({"intent":{"kind":"Tier","tier":"Mid"},"effort":"High"})
            );
            Ok(())
        });
        app.shutdown().await;
    });
}
