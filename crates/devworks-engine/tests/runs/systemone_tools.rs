//! Home's native provider continuation for explicit SystemOne tool calls.
use super::auto_routing::{seed_systemone, systemone_ports};
use super::workspace_reply::{open_home, send_workspace, wait_for_replies};
use super::*;
use devworks_core::ports::provider_http::{SendPhase, TransportFailure};
use devworks_core::ports::{AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization};
use devworks_core::services::router::decisions::{DecisionIntent, OPERATION};
use devworks_engine::WorkspaceReplyStatus;
use tokio::sync::Notify;

const CALL: &str = "home-decision";
const ALIAS: &str = "router_decide_command";
const DECISION: &str = r#"{"model":"decision-a-upstream","answers":{"category":{"type":"choice","choice":"code","probabilities":{"code":0.9,"other":0.1},"confidence":0.9}},"usage":{"input_tokens":7,"output_tokens":0}}"#;

struct DecisionWire {
    model: &'static str,
    chat: Mutex<Vec<Value>>,
    decisions: AtomicUsize,
    sent: Notify,
    followed_up: Notify,
    release: (Mutex<bool>, Condvar),
}

impl DecisionWire {
    fn new(model: &'static str) -> Self {
        Self {
            model,
            chat: Mutex::default(),
            decisions: AtomicUsize::new(0),
            sent: Notify::new(),
            followed_up: Notify::new(),
            release: (Mutex::new(false), Condvar::new()),
        }
    }

    fn release_decision(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
    }

    fn result(&self) -> String {
        let chat = self.chat.lock().unwrap();
        let [first, followup] = chat.as_slice() else {
            panic!("expected two Home provider requests, got {}", chat.len());
        };
        assert!(
            first["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| { tool["function"]["name"] == ALIAS })
        );
        followup["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == CALL)
            .unwrap_or_else(|| panic!("missing the result for {CALL}: {followup}"))["content"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

impl StreamingHttp for DecisionWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        cancel: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let payload: Value = serde_json::from_slice(request.body.as_deref().unwrap()).unwrap();
        if request.url.ends_with("/systemone") {
            self.decisions.fetch_add(1, Ordering::SeqCst);
            self.sent.notify_one();
            let (released, signal) = &self.release;
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            let mut released = released.lock().unwrap();
            while !*released {
                if cancel.is_cancelled() || std::time::Instant::now() >= deadline {
                    return Err(TransportError {
                        phase: SendPhase::AfterSend,
                        kind: if cancel.is_cancelled() {
                            TransportFailure::Cancelled
                        } else {
                            TransportFailure::Deadline
                        },
                    });
                }
                (released, _) = signal
                    .wait_timeout(released, Duration::from_millis(20))
                    .unwrap();
            }
            return Ok(Box::new(AgentStream {
                head: ResponseHead {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                },
                body: Some(DECISION.as_bytes().to_vec()),
            }));
        }
        let index = {
            let mut chat = self.chat.lock().unwrap();
            chat.push(payload);
            chat.len()
        };
        let (delta, finish) = if index == 1 {
            let arguments = json!({
                "modelId":self.model,"expectedModelRevision":"1",
                "state":{"prompt":"Classify this"},"questions":[{"name":"category",
                    "question":{"kind":"Choice","instructions":"Which?",
                    "criteria":[{"name":"code"},{"name":"other"}]}}],
                "images":[],"videos":[],"timeoutMs":"30000",
            });
            (
                json!({"tool_calls":[{"index":0,"id":CALL,"type":"function",
                "function":{"name":ALIAS,"arguments":arguments.to_string()}}]}),
                "tool_calls",
            )
        } else {
            self.followed_up.notify_one();
            (json!({"content":"Finished the Home reply"}), "stop")
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

fn fixture() -> Fx {
    let f = Fx::new();
    seed_local_route(&f);
    seed_systemone(&f);
    f
}

fn send_decision(f: &Fx, app: &devworks_engine::Runtime) -> Id {
    let session = open_home(f, app);
    send_workspace(
        f,
        app,
        &session,
        CALL,
        "Use the decision tool",
        "watch-model",
    );
    session
}

async fn linked_decision(f: &Fx, session: &Id) -> StoredToolCall {
    within("Home records the decision's tool receipt", async {
        loop {
            if let Some(call) = read(&*f.store, |r| r.sessions().linked_operations(session, 10))
                .into_iter()
                .find(|call| call.native_call_id == CALL)
            {
                return call;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
}

fn operation_update(wire: &DecisionWire, operation: &OperationId, state: OperationState) -> Value {
    let result = wire.result();
    let update = result
        .lines()
        .filter_map(|line| line.strip_prefix("Tool operation update: "))
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|update| {
            update["operationId"] == json!(operation) && update["state"] == json!(state)
        });
    update.unwrap_or_else(|| panic!("missing {state:?} for {operation:?} in {result}"))
}

fn assert_decision_result(wire: &DecisionWire, operation: &OperationId) {
    let update = operation_update(wire, operation, OperationState::ObservedComplete);
    let result = &update["untrustedRemoteContent"]["result"]["value"];
    assert_eq!(result["modelId"], "decision-a");
    assert_eq!(
        result["output"]["value"],
        serde_json::from_str::<Value>(DECISION).unwrap()
    );
    assert_eq!(wire.decisions.load(Ordering::SeqCst), 1);
}

#[test]
fn home_waits_for_an_explicit_systemone_tool_result() {
    let _native_case = native_runtime_case();
    let f = fixture();
    let wire = Arc::new(DecisionWire::new("decision-a"));
    runtime().block_on(async {
        let app = boot(systemone_ports(&f, wire.clone())).await.unwrap();
        let session = send_decision(&f, &app);
        within("SystemOne receives the request", wire.sent.notified()).await;
        let operation = linked_decision(&f, &session).await.operation_id.unwrap();
        within("Home publishes the tool continuation", async {
            loop {
                let head = read(&*f.store, |r| r.sessions().provider_context_head(&session));
                if head.is_some_and(|head| head.kind == "ProviderTurnContinuation") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        let followed_up_early =
            tokio::time::timeout(Duration::from_secs(1), wire.followed_up.notified())
                .await
                .is_ok();
        assert_eq!(
            read(&*f.store, |r| r.operations().operation(&operation))
                .unwrap()
                .state,
            OperationState::Dispatching
        );
        wire.release_decision();
        wait_for_replies(&f, 1).await;
        assert!(
            !followed_up_early,
            "Home generated while the decision was still running"
        );
        assert_decision_result(&wire, &operation);
        app.shutdown().await;
    });
}

#[test]
fn home_reports_an_unknown_systemone_outcome_after_cancellation() {
    let _native_case = native_runtime_case();
    let f = fixture();
    let wire = Arc::new(DecisionWire::new("decision-a"));
    runtime().block_on(async {
        let app = boot(systemone_ports(&f, wire.clone())).await.unwrap();
        let session = send_decision(&f, &app);
        within("SystemOne sends the request", wire.sent.notified()).await;
        let operation = linked_decision(&f, &session).await.operation_id.unwrap();
        let decision = read(&*f.store, |r| r.operations().operation(&operation)).unwrap();
        let intent: DecisionIntent = serde_json::from_value(decision.payload).unwrap();
        f.tx(move |_, tx| tx.queue().cancel(&intent.delivery_id));
        app.dispatcher().wakeup().wake();
        wait_for_replies(&f, 1).await;
        let update = operation_update(&wire, &operation, OperationState::OutcomeUnknown);
        assert!(update["untrustedRemoteContent"].is_null());
        assert_eq!(wire.decisions.load(Ordering::SeqCst), 1);
        app.shutdown().await;
    });
}

struct DecisionNeedsApproval;

impl AuthorityPort for DecisionNeedsApproval {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        if request.descriptor.name == OPERATION && ctx.principal_kind() == PrincipalKind::Agent {
            return Err(AuthorityDenial::Policy(
                devworks_core::authority::DenialReason::ApprovalRequired,
            ));
        }
        Authority.authorize(ctx, request)
    }
}

#[test]
fn home_reports_refused_and_approval_blocked_systemone_calls() {
    let _native_case = native_runtime_case();
    runtime().block_on(async {
        let f = fixture();
        let wire = Arc::new(DecisionWire::new("missing-decision"));
        let app = boot(systemone_ports(&f, wire.clone())).await.unwrap();
        send_decision(&f, &app);
        wait_for_replies(&f, 1).await;
        assert!(
            wire.result().contains("domain_NotFound"),
            "{}",
            wire.result()
        );
        assert_eq!(wire.decisions.load(Ordering::SeqCst), 0);
        app.shutdown().await;

        let f = fixture();
        let wire = Arc::new(DecisionWire::new("decision-a"));
        let mut ports = systemone_ports(&f, wire.clone());
        ports.authority = Arc::new(DecisionNeedsApproval);
        let app = boot(ports).await.unwrap();
        let session = send_decision(&f, &app);
        wait_for_replies(&f, 1).await;
        // This operation has no saved-approval owner. A host authority denial is a rejected
        // command, with no accepted effect or approval identity to wait for.
        let result = wire.result();
        assert!(result.contains("command_PermissionDenied"), "{result}");
        assert!(read(&*f.store, |r| r.sessions().linked_operations(&session, 10)).is_empty());
        assert_eq!(wire.decisions.load(Ordering::SeqCst), 0);
        app.shutdown().await;
    });
}

#[test]
fn home_recovers_the_same_settled_systemone_tool_after_restart() {
    use devworks_core::ports::files::FileFault;

    let _native_case = native_runtime_case();
    let f = fixture();
    let wire = Arc::new(DecisionWire::new("decision-a"));
    let rt = runtime();
    let (session, original) = rt.block_on(async {
        let mut ports = systemone_ports(&f, wire.clone());
        let files = FaultFiles::over(Arc::new(NativeArtifactFiles::new(
            f.h.root.clone(),
            f.h.root.with_extension("cache"),
        )));
        let store = Arc::downgrade(&f.store);
        files.before(FileOp::Publish, move |call| {
            let operation = call
                .name
                .strip_suffix(".part")
                .and_then(|id| Id::new(id).ok());
            if let (Some(operation), Some(store)) = (operation, store.upgrade()) {
                let write = read(&*store, |r| {
                    r.operations().operation(&OperationId(operation))
                });
                if write
                    .is_some_and(|op| op.payload["source"]["kind"] == "ProviderTurnContinuation")
                {
                    return Err(FileFault::DiskFull);
                }
            }
            Ok(())
        });
        ports.artifacts = ArtifactComposition::new(
            files,
            Arc::new(HostArtifactSources::new(f.h.clock(), f.h.ids.clone())),
            Arc::new(NativeFilesystem::new()),
        );
        let app = boot(ports).await.unwrap();
        let session = send_decision(&f, &app);
        within("SystemOne receives the request", wire.sent.notified()).await;
        wire.release_decision();
        let original = linked_decision(&f, &session).await;
        let operation = original.operation_id.as_ref().unwrap();
        within(
            "the decision settles while the Home continuation needs recovery",
            async {
                loop {
                    let decision =
                        read(&*f.store, |r| r.operations().operation(operation)).unwrap();
                    if decision.state == OperationState::ObservedComplete
                        && app.workspace_reply_status(&f.user, &session).unwrap()
                            == Some(WorkspaceReplyStatus::NeedsReconciliation)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            },
        )
        .await;
        assert_eq!(wire.chat.lock().unwrap().len(), 1);
        app.shutdown().await;
        (session, original)
    });
    drop(rt);
    let (h, old_store) = f.into_parts();
    drop(old_store);
    h.wait_released();
    let store = h.open();
    let f = Fx::over(h, store);
    runtime().block_on(async {
        let app = boot(systemone_ports(&f, wire.clone())).await.unwrap();
        wait_for_replies(&f, 1).await;
        let recovered = read(&*f.store, |r| r.sessions().linked_operations(&session, 10));
        assert_eq!(recovered.as_slice(), std::slice::from_ref(&original));
        assert_decision_result(&wire, original.operation_id.as_ref().unwrap());
        app.shutdown().await;
    });
}
