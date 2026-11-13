//! A native agent turn whose tool call is refused `ApprovalRequired` holds that call. While it
//! waits, the turn does not submit the call again on a timer: it submits it once, and again only
//! after an admitted command changed state (an approval decision is one) or the turn is stopped.
//! Provider bytes are scripted; no model runs.
use super::*;
use devworks_core::ports::provider_http::{
    HttpStream, ResponseHead, StreamCloser, StreamLimits, StreamingHttp, TransportError,
};
use devworks_core::ports::{AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization};
use devworks_core::protocol::CorrelationId;
use devworks_core::protocol::records::RouteIntent;
use devworks_core::services::sessions::Refusal;
use std::sync::OnceLock;

/// The tool the scripted provider calls. Its handler refuses every new request for approval.
const HELD: &str = "agents.ack";

struct NeedsApproval;
impl devworks_core::ports::CommandHandler for NeedsApproval {
    fn decide(
        &self,
        _: &InvocationContext,
        _: &devworks_core::gateway::AdmittedCommand,
        _: &mut dyn WriteTransaction,
    ) -> Result<devworks_core::ports::Decided, devworks_core::ports::HandlerFailure> {
        Ok(devworks_core::ports::Decided {
            decision: DomainDecision::Refused {
                fault: Refusal::new(
                    devworks_core::protocol::DomainErrorCode::ApprovalRequired,
                    "fixture_review",
                    None,
                )
                .fault(CorrelationId(id("approval-wait-fault"))),
            },
            operation_id: None,
        })
    }
}

/// Counts every gateway submission of [`HELD`]: the gateway asks authority before it records or
/// replays a receipt, so a replay is counted too.
struct Counting {
    submissions: Arc<AtomicUsize>,
}
impl AuthorityPort for Counting {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        if request.descriptor.name == HELD {
            self.submissions.fetch_add(1, Ordering::SeqCst);
        }
        Authority.authorize(ctx, request)
    }
}

/// The first request gets one call to the held tool; every later request gets a final answer.
struct HeldWire {
    requests: AtomicUsize,
}
struct Body(Option<Vec<u8>>);
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
        _: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let wire = if self.requests.fetch_add(1, Ordering::SeqCst) == 0 {
            let call = json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,
                "id":"call-held","type":"function","function":{"name":"agents_ack_command",
                "arguments":"{}"}}]},"finish_reason":null}]});
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

#[test]
fn a_tool_call_awaiting_approval_is_submitted_once_and_again_only_after_a_decision() {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    let submissions = Arc::new(AtomicUsize::new(0));
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut composed = composition_with(
                &fx,
                None,
                Arc::new(FakePower::default()),
                Arc::new(Counting {
                    submissions: submissions.clone(),
                }),
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
            let mut use_cases = UseCases::new()
                .with_runs(fx.service.clone())
                .with_connections(connections.clone())
                .with_router(Arc::new(
                    devworks_core::services::router::InferenceRouter::new(
                        fx.h.ids.clone(),
                        fx.h.clock(),
                        connections,
                    ),
                ));
            use_cases.register(HELD, None, Some(Arc::new(NeedsApproval)));
            composed.use_cases = use_cases;
            composed.config.providers = Some(
                devworks_engine::providers::driver::ProviderComposition::new(Arc::new(HeldWire {
                    requests: AtomicUsize::new(0),
                })),
            );
            let app = Arc::new(boot(composed).await.unwrap());

            // A coordinator agent session pinned to a tool-capable model.
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
            let assignment =
                read(&*fx.store, |s| s.runs().assignment(&primary.assignment_id)).unwrap();
            let session =
                read(&*fx.store, |s| s.sessions().session(&assignment.session_id)).unwrap();
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
            let offered: Vec<_> = app
                .providers()
                .unwrap()
                .schemas()
                .selected_agent_tools(&context)
                .unwrap()
                .into_iter()
                .map(|tool| tool.wire_name)
                .collect();
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

            // A person's decision is an admitted command that changes state. The turn wakes
            // on it and submits the held call again, well before its 30 s recheck.
            let decision = json!({"protocol": PROTOCOL, "requestKey": fx.key(), "command": {
                "kind": "runs.start", "rootId": "root-g", "expectedWorkRevision": "1",
                "expectedControlRevision": "1"}});
            let decided = app
                .command(&fx.user, "runs.start", decision.to_string().as_bytes())
                .unwrap();
            assert!(
                matches!(&decided, CommandReply::Recorded { receipt, .. }
                    if matches!(receipt.decision, DomainDecision::Applied { .. })),
                "{decided:?}"
            );
            assert!(
                reached(&submissions, 2, Duration::from_secs(10)).await,
                "the decision did not resume the held call"
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
