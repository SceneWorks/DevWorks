//! Native agent tool calls that wait. A call refused for a reason that clears later is held: the
//! turn submits it once, and again only after an admitted command changed state or the turn is
//! stopped. A call a domain command refuses `ApprovalRequired` is not held: it is answered at once
//! with an approval staged for the exact refused request, and the person's decision reaches the
//! agent later as an update of the call's operation. Provider bytes are scripted; no model runs.
use super::*;
use devworks_core::ports::desktop::{
    DesktopAvailability, DesktopCapabilities, DesktopFault, DesktopPlatform,
};
use devworks_core::ports::provider_http::{
    HttpStream, ResponseHead, StreamCloser, StreamLimits, StreamingHttp, TransportError,
};
use devworks_core::ports::{AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization};
use devworks_core::protocol::CorrelationId;
use devworks_core::protocol::provider::TranscriptBody;
use devworks_core::protocol::records::{AgentRecord, DesktopInput, RouteIntent};
use devworks_core::services::sessions::Refusal;
use std::sync::{Mutex, OnceLock};

/// The tool the scripted provider calls in the held-call test. Its handler refuses every new
/// request until capacity frees up, so the call is held.
const HELD: &str = "agents.ack";

struct NeedsCapacity;
impl devworks_core::ports::CommandHandler for NeedsCapacity {
    fn decide(
        &self,
        _: &InvocationContext,
        _: &devworks_core::gateway::AdmittedCommand,
        _: &mut dyn WriteTransaction,
    ) -> Result<devworks_core::ports::Decided, devworks_core::ports::HandlerFailure> {
        Ok(devworks_core::ports::Decided {
            decision: DomainDecision::Refused {
                fault: Refusal::new(
                    devworks_core::protocol::DomainErrorCode::CapacityWait,
                    "fixture_capacity",
                    None,
                )
                .retry(devworks_core::protocol::RetryAdvice::AfterBlockerClears)
                .fault(CorrelationId(id("approval-wait-fault"))),
            },
            operation_id: None,
        })
    }
}

/// Counts every gateway submission of `counted`: the gateway asks authority before it records
/// or replays a receipt, so a replay is counted too. `real` uses the host's own authority (with
/// reviewed grants); otherwise the permissive fixture authority decides.
struct Counting {
    counted: &'static str,
    submissions: Arc<AtomicUsize>,
    real: Option<(
        Arc<dyn ApplicationStore>,
        Arc<dyn devworks_core::ports::Clock>,
    )>,
}
impl AuthorityPort for Counting {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        if request.descriptor.name == self.counted {
            self.submissions.fetch_add(1, Ordering::SeqCst);
        }
        match &self.real {
            Some((store, clock)) => HostAuthority {
                store: &**store,
                clock: &**clock,
                protected_branches: &[],
            }
            .authorize(ctx, request),
            None => Authority.authorize(ctx, request),
        }
    }
}

/// The first request gets `calls` identical calls to `call` (a wire name and its JSON arguments);
/// every later request gets a final answer. Every request body is kept.
struct HeldWire {
    call: Mutex<(String, String)>,
    calls: usize,
    bodies: Mutex<Vec<Vec<u8>>>,
}
impl HeldWire {
    fn new(name: &str, arguments: &str, calls: usize) -> Self {
        Self {
            call: Mutex::new((name.into(), arguments.into())),
            calls,
            bodies: Mutex::new(Vec::new()),
        }
    }
}
pub(super) struct Body(pub(super) Option<Vec<u8>>);
struct Close;
impl StreamCloser for Close {
    fn close(&self) -> Result<(), TransportError> {
        Ok(())
    }
}
impl HttpStream for Body {
    fn closer(&self) -> Arc<dyn StreamCloser> {
        Arc::new(Close)
    }
    fn head(&self) -> &ResponseHead {
        static HEAD: OnceLock<ResponseHead> = OnceLock::new();
        HEAD.get_or_init(|| ResponseHead {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
        })
    }
    fn read(&mut self, buffer: &mut [u8], _: &Cancellation) -> Result<usize, TransportError> {
        let Some(mut body) = self.0.take() else {
            return Ok(0);
        };
        let n = body.len().min(buffer.len());
        buffer[..n].copy_from_slice(&body[..n]);
        if n < body.len() {
            self.0 = Some(body.split_off(n));
        }
        Ok(n)
    }
    fn close(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
}
impl StreamingHttp for HeldWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let first = {
            let mut bodies = self.bodies.lock().unwrap();
            bodies.push(request.body.clone().unwrap_or_default());
            bodies.len() == 1
        };
        let wire = if first {
            let (name, arguments) = self.call.lock().unwrap().clone();
            let calls: Vec<_> = (0..self.calls)
                .map(|n| {
                    json!({"index":n,"id":format!("call-held-{n}"),"type":"function",
                        "function":{"name":name,"arguments":arguments}})
                })
                .collect();
            let call = json!({"choices":[{"index":0,"delta":{"tool_calls":calls},
                "finish_reason":null}]});
            format!(
                "data: {call}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\
                 \"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
            )
        } else {
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\
             \"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                .to_owned()
        };
        Ok(Box::new(Body(Some(wire.into_bytes()))))
    }
}

struct Entropy;
impl devworks_core::ports::http::Entropy for Entropy {
    fn fill(&self, bytes: &mut [u8]) {
        bytes.fill(7);
    }
}

/// Waits (bounded) until `count` reaches `at_least`.
async fn reached(count: &AtomicUsize, at_least: usize, bound: Duration) -> bool {
    let deadline = std::time::Instant::now() + bound;
    while count.load(Ordering::SeqCst) < at_least {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    true
}

/// A runtime whose provider is `wire`, with `adjust` applied to its composition.
pub(super) async fn runtime_with(
    fx: &Fx,
    authority: Arc<dyn AuthorityPort>,
    wire: Arc<dyn StreamingHttp>,
    adjust: impl FnOnce(&mut Composition),
) -> Arc<devworks_engine::Runtime> {
    let mut composed = composition_with(
        fx,
        None,
        Arc::new(FakePower::default()),
        authority,
        Arc::new(FakeHost::default()),
    );
    let connections = Arc::new(
        devworks_core::services::router::connections::ConnectionService::new(
            fx.h.ids.clone(),
            fx.h.clock(),
            fx.store.clone(),
            Arc::new(devworks_core::ports::http::NoNetwork),
            Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
            Arc::new(Entropy),
        ),
    );
    composed.use_cases = UseCases::new()
        .with_runs(fx.service.clone())
        .with_connections(connections.clone())
        .with_router(Arc::new(
            devworks_core::services::router::InferenceRouter::new(
                fx.h.ids.clone(),
                fx.h.clock(),
                connections,
            ),
        ));
    composed.config.providers =
        Some(devworks_engine::providers::driver::ProviderComposition::new(wire));
    adjust(&mut composed);
    Arc::new(boot(composed).await.unwrap())
}

/// A coordinator agent session pinned to a tool-capable model: the agent's invocation context
/// and its record.
pub(super) fn pinned_agent_session(fx: &Fx) -> (InvocationContext, AgentRecord) {
    applied(&fx.start(&fx.user, "root-i", 1, 1));
    let activation = fx.launch("approval-wait-owner");
    let host = principal(&*fx.store, "approval-wait-host", PrincipalKind::Host);
    let agents = AgentService::new(fx.h.ids.clone(), fx.service.clone());
    let (execution_id, epoch) = (activation.id.clone(), activation.owner_epoch);
    let primary = fx.tx(move |_, tx| {
        agents
            .materialize_primary(tx, &host, &execution_id, epoch)
            .map_err(|error| StorageError::InvalidRecord(format!("primary: {error:?}")))
    });
    let assignment = read(&*fx.store, |s| s.runs().assignment(&primary.assignment_id)).unwrap();
    let session = read(&*fx.store, |s| s.sessions().session(&assignment.session_id)).unwrap();
    let context = devworks_core::context::HostIngress::new(&*fx.store)
        .context(
            session.agent_id.clone(),
            PrincipalKind::Agent,
            id("approval-wait-invocation"),
            id("approval-wait-ingress"),
            devworks_core::protocol::Counter::new(fx.store.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_session(session.id.clone())
        .with_lineage(
            session.project_id.clone(),
            session.root_id.clone(),
            Some(primary.assignment_id.clone()),
        )
        .with_activation(activation.id.clone(), epoch);
    let pinned = session.id.clone();
    fx.tx(move |_, tx| {
        tx.router().put_connection(
            &StoredConnection {
                id: id("approval-wait-connection"),
                name: "Fixture".into(),
                profile: "CompatibleChat".into(),
                endpoint: "http://localhost:11434/v1".into(),
                enabled: true,
                local_free: true,
                open_router: None,
                credential_ref: None,
                credential_revision: 0,
                credential_epoch: None,
                lifecycle: "Connected".into(),
                state_reason: None,
                account_revision: 1,
                refresh_operation_id: None,
                account_digest: None,
                access_expires_at_ms: None,
                risk: None,
                discovered: None,
                discovered_at_ms: None,
                healthy: Some(true),
                checked_at_ms: Some(0),
                revision: 1,
                created_at_ms: 0,
                updated_at_ms: 0,
            },
            None,
        )?;
        tx.router().put_route_model(
            &StoredRouteModel {
                id: id("approval-wait-model"),
                display_name: "Fixture model".into(),
                endpoint_profile: "CompatibleChat".into(),
                upstream_model_id: "fixture-model".into(),
                enabled: true,
                efforts: vec![],
                image_input: false,
                tools: true,
                context_tokens: Some(16_000),
                connection_id: Some(id("approval-wait-connection")),
                position: 1,
                revision: 1,
                updated_at_ms: 0,
            },
            None,
        )?;
        tx.router().put_session_pin(
            &StoredPin {
                session_id: pinned,
                pin_id: id("approval-wait-pin"),
                revision: 1,
                model_id: id("approval-wait-model"),
                connection_id: id("approval-wait-connection"),
                upstream_model_id: Some("fixture-model".into()),
                endpoint_profile: Some("CompatibleChat".into()),
                identity_origin: "Resolved".into(),
                intent: serde_json::to_string(&RouteIntent::Model {
                    model_id: id("approval-wait-model"),
                })
                .unwrap(),
                requested_effort: "Default".into(),
                effective_effort: "Default".into(),
                required_capabilities: vec!["tools".into()],
                replaced_pin_id: None,
                reason: "Initial".into(),
                updated_at_ms: 0,
            },
            None,
        )?;
        Ok(())
    });
    (context, primary)
}

/// The wire names of the tools the agent is offered.
fn offered(app: &devworks_engine::Runtime, context: &InvocationContext) -> Vec<String> {
    app.providers()
        .unwrap()
        .schemas()
        .selected_agent_tools(context)
        .unwrap()
        .into_iter()
        .map(|tool| tool.wire_name)
        .collect()
}

#[test]
fn a_held_tool_call_is_submitted_once_and_again_only_after_a_state_change() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let submissions = Arc::new(AtomicUsize::new(0));
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let app = runtime_with(
                &fx,
                Arc::new(Counting {
                    counted: HELD,
                    submissions: submissions.clone(),
                    real: None,
                }),
                Arc::new(HeldWire::new("agents_ack_command", "{}", 1)),
                |composed| {
                    composed
                        .use_cases
                        .register(HELD, None, Some(Arc::new(NeedsCapacity)));
                },
            )
            .await;
            let (context, primary) = pinned_agent_session(&fx);
            let offered = offered(&app, &context);
            assert!(
                offered.iter().any(|name| name == "agents_ack_command"),
                "{offered:?}"
            );

            let (stop_tx, stop) = tokio::sync::watch::channel(false);
            let turn = tokio::spawn(NativeAgentTurnExecutor::new(&app).run(
                context.clone(),
                primary.clone(),
                stop,
            ));
            // Boot-time Help generation can hold the single store writer for several seconds in
            // a debug build; this bound only catches a hang.
            assert!(
                reached(&submissions, 1, Duration::from_secs(180)).await,
                "the held call was never submitted"
            );

            // Nothing changes for 10 s: the held call is not submitted again.
            tokio::time::sleep(Duration::from_secs(10)).await;
            assert_eq!(submissions.load(Ordering::SeqCst), 1);
            assert!(!turn.is_finished());

            // An admitted command that changes state wakes the turn, which submits the held
            // call again, well before its 30 s recheck.
            let change = json!({"protocol": PROTOCOL, "requestKey": fx.key(), "command": {
                "kind": "runs.start", "rootId": "root-g", "expectedWorkRevision": "1",
                "expectedControlRevision": "1"}});
            let changed = app
                .command(&fx.user, "runs.start", change.to_string().as_bytes())
                .unwrap();
            assert!(
                matches!(&changed, CommandReply::Recorded { receipt, .. }
                    if matches!(receipt.decision, DomainDecision::Applied { .. })),
                "{changed:?}"
            );
            assert!(
                reached(&submissions, 2, Duration::from_secs(10)).await,
                "the state change did not resume the held call"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
            assert_eq!(submissions.load(Ordering::SeqCst), 2);

            // A stop ends the wait at once rather than at the next recheck.
            stop_tx.send_replace(true);
            let end = tokio::time::timeout(Duration::from_secs(10), turn)
                .await
                .expect("the stopped turn ended within 10 s")
                .unwrap();
            assert!(
                matches!(
                    end,
                    LaunchEnd::Finished {
                        outcome: ActivationOutcome::Cancelled,
                        ..
                    }
                ),
                "{end:?}"
            );
            Arc::try_unwrap(app)
                .ok()
                .expect("runtime has one owner")
                .shutdown()
                .await;
        });
}

/// A desktop whose input can be taken over: `desktop.request_control` needs a person's OK, so a
/// domain command refuses an agent's request `ApprovalRequired`.
struct ScriptedDesktop;
impl DesktopPlatform for ScriptedDesktop {
    fn capabilities(&self) -> DesktopCapabilities {
        DesktopCapabilities {
            target: "test-scripted".into(),
            capture: DesktopAvailability::Unsupported("capture-not-scripted"),
            input: DesktopAvailability::Available,
            accessibility: DesktopAvailability::Unsupported("accessibility-not-scripted"),
        }
    }
    fn capture_png(&self) -> Result<Vec<u8>, DesktopFault> {
        Err(DesktopFault::Unsupported)
    }
    fn inject(&self, _: &DesktopInput) -> Result<(), DesktopFault> {
        Err(DesktopFault::Unsupported)
    }
}

const TAKEOVER: &str = "desktop.request_control";

/// A booted runtime with a scripted desktop, the host's own authority and the desktop takeover
/// tool selected for agents; the agent's context and record; and the takeover arguments.
struct Takeover {
    app: Arc<devworks_engine::Runtime>,
    wire: Arc<HeldWire>,
    context: InvocationContext,
    primary: AgentRecord,
    arguments: Value,
}

async fn takeover(fx: &Fx, calls: usize) -> Takeover {
    let descriptor = devworks_core::settings::catalog::descriptor("tools.overrides").unwrap();
    let schema = descriptor.schema_ref();
    fx.tx(move |_, tx| {
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            "settings",
            1,
            1,
            json!({"values": {"tools.overrides": {"schema": schema,
                "value": {"devworks/desktop.request_control": true}}}}),
        )?;
        Ok(())
    });
    let wire = Arc::new(HeldWire::new(
        "desktop_request_control_command",
        "{}",
        calls,
    ));
    let app = runtime_with(
        fx,
        Arc::new(Counting {
            counted: TAKEOVER,
            submissions: Arc::new(AtomicUsize::new(0)),
            real: Some((fx.store.clone(), fx.h.clock())),
        }),
        wire.clone(),
        |composed| {
            composed.config.desktop = Some(devworks_engine::runtime::DesktopComposition {
                platform: Arc::new(ScriptedDesktop),
            });
        },
    )
    .await;
    let (context, primary) = pinned_agent_session(fx);
    let offered = offered(&app, &context);
    assert!(
        offered
            .iter()
            .any(|name| name == "desktop_request_control_command"),
        "{offered:?}"
    );
    let audience = fx.user.principal().clone();
    fx.tx(move |_, tx| tx.decisions().bind_notification_user(&audience));
    let desktop = desktop_record(&app, &fx.user);
    let arguments = json!({"desktopId": desktop["desktopId"],
        "expectedRevision": desktop["revision"], "reason": "Test the local UI",
        "leaseMs": "30000"});
    *wire.call.lock().unwrap() = (
        "desktop_request_control_command".into(),
        arguments.to_string(),
    );
    Takeover {
        app,
        wire,
        context,
        primary,
        arguments,
    }
}

/// The JSON object (anywhere in `value`) that has a `desktopId`.
fn with_desktop_id(value: &Value) -> Option<Value> {
    match value {
        Value::Object(map) if map.contains_key("desktopId") => Some(value.clone()),
        Value::Object(map) => map.values().find_map(with_desktop_id),
        Value::Array(items) => items.iter().find_map(with_desktop_id),
        _ => None,
    }
}

fn desktop_record(app: &devworks_engine::Runtime, user: &InvocationContext) -> Value {
    let frame = json!({"protocol": PROTOCOL, "mode": "Query", "schemaRevision": 1,
        "queryId": "desktop-inspect", "body": {"kind": "desktop.inspect"}});
    let reply = app
        .query(user, "desktop.inspect", frame.to_string().as_bytes())
        .unwrap();
    with_desktop_id(&serde_json::to_value(&reply).unwrap())
        .unwrap_or_else(|| panic!("no desktop record: {reply:?}"))
}

/// Runs the agent's turn to its end (bounded), as the agent's run would.
async fn run_turn(t: &Takeover) -> LaunchEnd {
    let (_stop_tx, stop) = tokio::sync::watch::channel(false);
    // Boot-time Help generation can hold the single store writer for several seconds in a debug
    // build; this bound only catches a hang.
    tokio::time::timeout(
        Duration::from_secs(180),
        NativeAgentTurnExecutor::new(&t.app).run(t.context.clone(), t.primary.clone(), stop),
    )
    .await
    .expect("the turn was not held")
}

/// The tool results the agent's second request carried, by call ID.
fn tool_results(wire: &HeldWire) -> Vec<(String, String)> {
    let bodies = wire.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2, "one tool turn and one final answer");
    let request: Value = serde_json::from_slice(&bodies[1]).unwrap();
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| {
            (
                m["tool_call_id"].as_str().unwrap().to_owned(),
                m["content"].to_string(),
            )
        })
        .collect()
}

/// The approval and operation a tool call was answered with, from its durable response.
fn answered(fx: &Fx, context: &InvocationContext, call: &str) -> (Id, OperationId) {
    let session = context.session_id().unwrap().clone();
    let linked = read(&*fx.store, |s| {
        s.sessions().linked_operations(&session, 500)
    });
    let row = linked
        .iter()
        .find(|row| row.native_call_id == call)
        .unwrap_or_else(|| panic!("no linked call {call}: {linked:?}"));
    assert_eq!(row.outcome.as_deref(), Some("ApprovalRequired"));
    let operation = row.operation_id.clone().unwrap();
    let entries = read(&*fx.store, |s| s.sessions().entries(&session, 0, 500));
    let approval = entries
        .iter()
        .find_map(|entry| {
            match serde_json::from_value::<TranscriptBody>(entry.body.clone()).ok()? {
                TranscriptBody::ToolResponseRecorded {
                    call: key,
                    outcome:
                        devworks_core::protocol::provider::ToolOutcome::ApprovalRequired {
                            approval_id,
                            operation_id,
                        },
                } if key.native_call_id == call && operation_id == operation => Some(approval_id),
                _ => None,
            }
        })
        .unwrap();
    (approval, operation)
}

/// The operation updates appended to the agent's session for `operation`, oldest first, after
/// the one that reports it awaiting a person (the turn reconciles linked operations before its
/// next request).
fn decided(
    fx: &Fx,
    app: &devworks_engine::Runtime,
    context: &InvocationContext,
    operation: &OperationId,
) -> Vec<Value> {
    updates(fx, app, context, operation)
        .into_iter()
        .filter(|update| update["state"] != "IntentRecorded")
        .collect()
}

/// The operation updates appended to the agent's session for `operation`, oldest first.
fn updates(
    fx: &Fx,
    app: &devworks_engine::Runtime,
    context: &InvocationContext,
    operation: &OperationId,
) -> Vec<Value> {
    let session = context.session_id().unwrap().clone();
    read(&*fx.store, |s| s.sessions().entries(&session, 0, 500))
        .into_iter()
        .filter_map(
            |entry| match serde_json::from_value::<TranscriptBody>(entry.body).ok()? {
                TranscriptBody::OperationUpdate {
                    operation_id,
                    artifact,
                    ..
                } if operation_id == *operation => Some(artifact),
                _ => None,
            },
        )
        .map(|artifact| {
            let bytes = app
                .artifacts()
                .read_exact(&**app.store(), context, &artifact, 1 << 20)
                .unwrap()
                .0;
            serde_json::from_slice(&bytes).unwrap()
        })
        .collect()
}

fn decide(fx: &Fx, app: &devworks_engine::Runtime, approval: &Id, approve: bool) {
    let stored = read(&*fx.store, |s| s.decisions().approval(approval)).unwrap();
    let frame = json!({"mode": "Command", "protocol": PROTOCOL, "schemaRevision": 3,
        "requestKey": fx.key(), "body": {"kind": "approvals.record_human_decision",
        "proposalId": approval, "expectedRevision": stored.revision.to_string(),
        "contentDigest": stored.content_digest,
        "decision": if approve { "Approve" } else { "Reject" },
        "rationale": "Decided in the fixture"}});
    let reply = app
        .record_human_decision(&fx.user, frame.to_string().as_bytes())
        .unwrap();
    assert!(
        matches!(&reply, CommandReply::Recorded { receipt, .. }
            if matches!(receipt.decision, DomainDecision::Applied { .. })),
        "{reply:?}"
    );
}

fn in_runtime(test: impl AsyncFnOnce()) {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(test());
}

async fn shut_down(app: Arc<devworks_engine::Runtime>) {
    Arc::try_unwrap(app)
        .ok()
        .expect("runtime has one owner")
        .shutdown()
        .await;
}

#[test]
fn an_approval_required_call_is_answered_at_once_and_its_approved_outcome_reaches_the_agent() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    in_runtime(async || {
        let t = takeover(&fx, 1).await;
        let end = run_turn(&t).await;
        assert!(
            matches!(
                end,
                LaunchEnd::Finished {
                    outcome: ActivationOutcome::Completed,
                    ..
                }
            ),
            "{end:?}"
        );

        // The call was answered ApprovalRequired with the staged approval and its operation.
        let (approval_id, operation_id) = answered(&fx, &t.context, "call-held-0");
        let results = tool_results(&t.wire);
        assert_eq!(results.len(), 1);
        assert!(results[0].1.contains("ApprovalRequired"), "{results:?}");
        assert!(results[0].1.contains(approval_id.as_str()), "{results:?}");
        assert!(
            results[0].1.contains(operation_id.0.as_str()),
            "{results:?}"
        );

        // The approval is for exactly the refused request, by the agent, awaiting a person.
        let approval = read(&*fx.store, |s| s.decisions().approval(&approval_id)).unwrap();
        let reviewed =
            devworks_core::services::approvals::requests::decode(TAKEOVER, 3, &t.arguments, true)
                .unwrap();
        assert_eq!(approval.state, "Pending");
        assert_eq!(approval.operation_kind, TAKEOVER);
        assert_eq!(approval.content_digest, reviewed.fingerprint.as_str());
        assert_eq!(approval.author, *t.context.principal());
        assert!(decided(&fx, &t.app, &t.context, &operation_id).is_empty());

        // A person approves; the reviewed request is readmitted as the agent and takes effect.
        decide(&fx, &t.app, &approval_id, true);
        t.app.approval_resumes().tick().await.unwrap();
        let desktop = desktop_record(&t.app, &fx.user);
        assert_eq!(desktop["consentId"], json!(approval_id), "{desktop}");

        // Its outcome is appended to the agent's session as an update of the call's operation.
        let seen = decided(&fx, &t.app, &t.context, &operation_id);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0]["state"], "ObservedComplete", "{seen:?}");
        assert_eq!(seen[0]["approval"]["decision"], "Approved", "{seen:?}");
        let result = seen[0]["approval"]["result"].to_string();
        assert!(result.contains("Applied"), "{result}");
        shut_down(t.app).await;
    });
}

#[test]
fn a_repeated_call_asks_once_and_a_rejection_reaches_the_agent() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    in_runtime(async || {
        // The agent sends the same request twice; each is refused, and both are answered with
        // the one approval.
        let t = takeover(&fx, 2).await;
        run_turn(&t).await;
        let (first, operation) = answered(&fx, &t.context, "call-held-0");
        let (second, again) = answered(&fx, &t.context, "call-held-1");
        assert_eq!((&first, &operation), (&second, &again));
        let project = ConfigScope::Project(ProjectId(id("project-a")));
        let pending = read(&*fx.store, |s| {
            s.decisions().list_approval_sources(
                &ApprovalSourceFilter {
                    scope: project.clone(),
                    state: Some("Pending".into()),
                },
                None,
                100,
            )
        });
        assert_eq!(pending.len(), 1, "{pending:?}");

        // A person rejects it: nothing runs, and the agent is told.
        decide(&fx, &t.app, &first, false);
        let seen = decided(&fx, &t.app, &t.context, &operation);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0]["state"], "CancelledBeforeDispatch", "{seen:?}");
        assert_eq!(seen[0]["approval"]["decision"], "Rejected", "{seen:?}");
        assert!(desktop_record(&t.app, &fx.user)["consentId"].is_null());
        shut_down(t.app).await;
    });
}

#[test]
fn an_approval_whose_grant_expired_before_readmission_reaches_the_agent_as_failed() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    in_runtime(async || {
        let t = takeover(&fx, 1).await;
        run_turn(&t).await;
        let (approval, operation) = answered(&fx, &t.context, "call-held-0");
        decide(&fx, &t.app, &approval, true);
        // The one-use grant the approval minted expires before the readmission runs.
        fx.h.advance(devworks_core::authority::service::MAX_GRANT_LIFETIME_MS + 1);
        t.app.approval_resumes().tick().await.unwrap();
        assert!(desktop_record(&t.app, &fx.user)["consentId"].is_null());
        let seen = decided(&fx, &t.app, &t.context, &operation);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0]["state"], "Failed", "{seen:?}");
        assert_eq!(seen[0]["approval"]["decision"], "Approved", "{seen:?}");
        shut_down(t.app).await;
    });
}

#[test]
fn the_same_request_from_another_session_gets_its_own_approval_and_decision() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    in_runtime(async || {
        let t = takeover(&fx, 1).await;
        // A second session of the same agent, in the same work scope.
        let first = t.context.session_id().unwrap().clone();
        let mut other = read(&*fx.store, |s| s.sessions().session(&first)).unwrap();
        other.id = id("approval-wait-other-session");
        other.conversation_id = id("approval-wait-other-conversation");
        let inserted = other.clone();
        fx.tx(move |_, tx| tx.sessions().insert_session(&inserted));
        let elsewhere = t.context.clone().with_session(other.id.clone());

        let digest =
            devworks_core::services::approvals::requests::decode(TAKEOVER, 3, &t.arguments, true)
                .unwrap()
                .fingerprint;
        let stage = |ctx: &InvocationContext| {
            t.app
                .approvals()
                .stage_held(
                    &**t.app.store(),
                    ctx,
                    TAKEOVER,
                    3,
                    &t.arguments,
                    digest.as_str(),
                )
                .unwrap()
        };
        let a = stage(&t.context);
        let b = stage(&elsewhere);
        // A re-send from the same session still asks once; another session asks separately.
        assert_eq!(stage(&t.context), a);
        assert_eq!(stage(&elsewhere), b);
        assert_ne!(a.approval_id, b.approval_id);
        assert_ne!(a.operation_id, b.operation_id);

        // Each decision settles that session's own operation and is addressed to that session.
        decide(&fx, &t.app, &b.approval_id, false);
        decide(&fx, &t.app, &a.approval_id, true);
        t.app.approval_resumes().tick().await.unwrap();
        for (held, session, state) in [
            (&a, &first, OperationState::ObservedComplete),
            (&b, &other.id, OperationState::CancelledBeforeDispatch),
        ] {
            let (ctx, to, operation, observed) =
                devworks_core::services::approvals::held::held_target(
                    &**t.app.store(),
                    &held.approval_id,
                )
                .unwrap()
                .unwrap();
            assert_eq!(&to, session);
            assert_eq!(ctx.session_id(), Some(session));
            assert_eq!(operation, held.operation_id);
            assert_eq!(observed, state);
        }
        assert_eq!(
            desktop_record(&t.app, &fx.user)["consentId"],
            json!(a.approval_id)
        );
        shut_down(t.app).await;
    });
}
