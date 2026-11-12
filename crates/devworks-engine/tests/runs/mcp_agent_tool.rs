//! A native agent turn calls a human-selected tool of an in-process MCP server. The provider
//! request offers the tool, the runtime journals and dispatches the call through the real
//! native client, and the server's answer reaches the next provider request as tool-result
//! content. Provider bytes are scripted; no model runs.
use super::*;
use devworks_adapters::mcp::NativeMcpClients;
use devworks_core::ports::mcp::{McpEndpoint, RemoteCatalog, RemoteTool};
use devworks_core::ports::provider_http::{
    HttpStream, ResponseHead, StreamCloser, StreamLimits, StreamingHttp, TransportError,
};
use devworks_core::protocol::records::RouteIntent;
use devworks_core::services::mcp::McpClientService;
use devworks_core::settings::model as settings;
use std::sync::OnceLock;

const ANSWER: &str = "echo-server saw ticket DW-45";

/// Each call returns one text block naming the `ticket` argument it received. A call for the
/// ticket `BROKEN` fails with a protocol error instead.
struct EchoServer;
impl rmcp::ServerHandler for EchoServer {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        rmcp::model::ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let ticket = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("ticket"))
            .and_then(Value::as_str)
            .unwrap_or("none")
            .to_owned();
        if ticket == "BROKEN" {
            return Err(rmcp::ErrorData::internal_error(
                "echo-server cannot read BROKEN",
                None,
            ));
        }
        Ok(rmcp::model::CallToolResponse::Complete(
            rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(format!(
                "echo-server saw ticket {ticket}"
            ))]),
        ))
    }
}

/// The first request gets one call to `tool` per `(call id, ticket)` in `calls`, all in the same
/// response; every later request gets a final answer. Each request body is kept for inspection.
struct ToolWire {
    tool: Mutex<String>,
    calls: Vec<(&'static str, &'static str)>,
    bodies: Mutex<Vec<Value>>,
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
impl StreamingHttp for ToolWire {
    fn open(
        &self,
        request: &HttpRequest,
        _: &StreamLimits,
        _: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        let body = serde_json::from_slice(request.body.as_deref().unwrap_or(b"null"))
            .unwrap_or(Value::Null);
        let mut bodies = self.bodies.lock().unwrap();
        bodies.push(body);
        let wire = if bodies.len() == 1 {
            let tool = self.tool.lock().unwrap().clone();
            let calls: String = self
                .calls
                .iter()
                .enumerate()
                .map(|(index, (call_id, ticket))| {
                    let call = json!({"choices":[{"index":0,"delta":{"tool_calls":[{
                        "index":index,"id":call_id,"type":"function","function":{"name":tool,
                        "arguments":json!({"ticket":ticket}).to_string()}}]},
                        "finish_reason":null}]});
                    format!("data: {call}\n\n")
                })
                .collect();
            format!(
                "{calls}data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\
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
        bytes.fill(5);
    }
}

fn decision(
    app: &devworks_engine::Runtime,
    ctx: &InvocationContext,
    op: &str,
    mut body: Value,
) -> Value {
    body["kind"] = json!(op);
    let frame = json!({"mode":"Command","protocol":PROTOCOL,"schemaRevision":8,
        "requestKey":format!("agent-mcp-{}", op.replace(['.', '_'], "-")),"body":body});
    match app.command(ctx, op, frame.to_string().as_bytes()) {
        Ok(CommandReply::Recorded { receipt, .. })
            if matches!(receipt.decision, DomainDecision::Applied { .. }) =>
        {
            serde_json::to_value(receipt.decision).unwrap()
        }
        other => panic!("{op} was not applied: {other:?}"),
    }
}

/// What one agent turn sent to the provider and recorded for its session.
struct Turn {
    alias: String,
    bodies: Vec<Value>,
    /// The session's linked operations: capability, state and payload.
    operations: Vec<(String, OperationState, Value)>,
    /// The bodies of the session's `ToolResponseRecorded` entries.
    responses: Vec<Value>,
    session_id: Value,
}

/// Runs one agent turn whose first provider response calls the selected echo tool once per
/// `(call id, ticket)` of `calls`, and asserts the turn completed.
fn agent_turn(calls: Vec<(&'static str, &'static str)>) -> Turn {
    let _native_case = native_runtime_case();
    let fx = Fx::new();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let mut composed = composition(&fx, None, Arc::new(FakePower::default()));
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
            let service = Arc::new(McpClientService::new(fx.h.ids.clone()));
            composed.use_cases = UseCases::new()
                .with_connections(connections.clone())
                .with_router(Arc::new(
                    devworks_core::services::router::InferenceRouter::new(
                        fx.h.ids.clone(),
                        fx.h.clock(),
                        connections,
                    ),
                ))
                .with_mcp(service.clone());
            let wire = Arc::new(ToolWire {
                tool: Mutex::default(),
                calls,
                bodies: Mutex::default(),
            });
            composed.config.providers =
                Some(devworks_engine::providers::driver::ProviderComposition::new(wire.clone()));
            let clients: Arc<OnceLock<Arc<NativeMcpClients>>> = Arc::default();
            let installed = clients.clone();
            composed.config.mcp = Some(devworks_engine::runtime::McpComposition {
                validator: Arc::new(devworks_adapters::packages::JsonSchemaValidator),
                factory: Arc::new(move |supervisor, handle| {
                    let native = Arc::new(NativeMcpClients::new(
                        supervisor,
                        Default::default(),
                        Arc::new(devworks_core::ports::secrets::SessionSecrets::new()),
                        "agent-mcp".into(),
                        Arc::new(NativeFilesystem::new()),
                        ProtectedRoots::new(vec![], vec![], None),
                        vec![],
                        handle,
                    ));
                    let _ = installed.set(native.clone());
                    native
                }),
            });
            let app = Arc::new(boot(composed).await.unwrap());

            // A human configures the server; its catalog is published and the human selects
            // its tool for agents.
            let transport = json!({"kind":"Http","endpoint":"http://127.0.0.1:9/mcp"});
            let configured = decision(
                &app,
                &fx.user,
                "mcp.configure",
                json!({"scope":{"kind":"Workspace"},"name":"Echo server","transport":transport}),
            );
            let server_id = configured["result"]["value"]["id"]
                .as_str()
                .unwrap_or_else(|| panic!("{configured}"))
                .to_owned();
            let (catalog, server) = (service.clone(), id(&server_id));
            fx.tx(move |_, tx| {
                catalog.publish_remote_catalog(
                    tx,
                    &ConfigScope::Workspace,
                    &server,
                    &Revision::new(1).unwrap(),
                    RemoteCatalog {
                        tools: vec![RemoteTool {
                            name: "echo_ticket".into(),
                            description: "Echo a ticket key".into(),
                            input_schema: json!({"type":"object",
                                "properties":{"ticket":{"type":"string"}},
                                "required":["ticket"]}),
                            output_schema: None,
                            read_only_hint: true,
                        }],
                        resources: vec![],
                        prompts: vec![],
                    },
                )
            });
            let tool_id = read(&*fx.store, |s| {
                let body = s
                    .configuration()
                    .current(&ConfigScope::Workspace, "outgoing_mcp")?
                    .unwrap()
                    .body;
                Ok(body["servers"][0]["capabilities"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|cap| cap["kind"] == "tool")
                    .unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_owned())
            });
            decision(
                &app,
                &fx.user,
                "mcp.select_tools",
                json!({"scope":{"kind":"Workspace"},"serverId":server_id,
                    "expectedRevision":"2","expectedCatalogRevision":"2","toolIds":[tool_id]}),
            );
            let capability = format!("mcp/{}", tool_id.strip_prefix("mcp-").unwrap());
            fx.tx(move |_, tx| {
                let current = settings::current_layer(tx.configuration(), &ConfigScope::Workspace)?;
                let descriptor = devworks_core::settings::catalog::descriptor(
                    devworks_core::settings::catalog::TOOL_OVERRIDES_KEY,
                )
                .unwrap();
                let assignment = serde_json::from_value(json!({"key": descriptor.key,
                    "value": {"schema": descriptor.schema_ref(), "value": {(capability): true}}}))
                .unwrap();
                let plan = settings::plan(&current, &[assignment]);
                let body = settings::body(
                    &plan,
                    &ActorId(id("user-a")),
                    tx.now_ms(),
                    json!({"kind": "test"}),
                );
                tx.configuration().append_revision(
                    &ConfigScope::Workspace,
                    settings::NAMESPACE,
                    current.revision,
                    settings::BODY_SCHEMA_VERSION,
                    body,
                )
            });

            // The in-process server takes the place of the endpoint's connection.
            let (client_reader, server_writer) = tokio::io::duplex(64 * 1024);
            let (server_reader, client_writer) = tokio::io::duplex(64 * 1024);
            let remote = tokio::spawn(async move {
                rmcp::serve_server(EchoServer, (server_reader, server_writer))
                    .await
                    .expect("in-process server handshake")
            });
            clients
                .get()
                .expect("boot installed the native MCP client")
                .attach_pipes(
                    &McpEndpoint {
                        server_id: id(&server_id),
                        scope: ConfigScope::Workspace,
                        transport: serde_json::from_value(transport).unwrap(),
                        sampling: false,
                    },
                    client_reader,
                    client_writer,
                )
                .await
                .expect("attached server");
            let remote = remote.await.unwrap();

            // A coordinator agent session pinned to a tool-capable model.
            applied(&fx.start(&fx.user, "root-i", 1, 1));
            let activation = fx.launch("agent-mcp-owner");
            let host = principal(&*fx.store, "agent-mcp-host", PrincipalKind::Host);
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
                    id("agent-mcp-invocation"),
                    id("agent-mcp-ingress"),
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
                        id: id("agent-mcp-connection"),
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
                        id: id("agent-mcp-model"),
                        display_name: "Fixture model".into(),
                        endpoint_profile: "CompatibleChat".into(),
                        upstream_model_id: "fixture-model".into(),
                        enabled: true,
                        efforts: vec![],
                        image_input: false,
                        tools: true,
                        context_tokens: Some(16_000),
                        connection_id: Some(id("agent-mcp-connection")),
                        position: 1,
                        revision: 1,
                        updated_at_ms: 0,
                    },
                    None,
                )?;
                tx.router().put_session_pin(
                    &StoredPin {
                        session_id: pinned,
                        pin_id: id("agent-mcp-pin"),
                        revision: 1,
                        model_id: id("agent-mcp-model"),
                        connection_id: id("agent-mcp-connection"),
                        upstream_model_id: Some("fixture-model".into()),
                        endpoint_profile: Some("CompatibleChat".into()),
                        identity_origin: "Resolved".into(),
                        intent: serde_json::to_string(&RouteIntent::Model {
                            model_id: id("agent-mcp-model"),
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

            // The agent sees the selected tool under its provider alias.
            let alias = read(&*fx.store, |s| service.selected_tool_materials(&context, s))
                .into_iter()
                .find(|material| material.wire_name == "echo_ticket")
                .expect("selected agent tool")
                .alias
                .wire_name;
            *wire.tool.lock().unwrap() = alias.clone();

            // Boot-time Help generation holds the single store writer for several seconds in a
            // debug build, so each turn write can wait behind it; this bound only catches a hang.
            let (_stop_tx, stop) = tokio::sync::watch::channel(false);
            let end = tokio::time::timeout(
                std::time::Duration::from_secs(180),
                NativeAgentTurnExecutor::new(&app).run(context.clone(), primary.clone(), stop),
            )
            .await
            .expect("the agent turn finished within 180 s");
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

            let operations = read(&*fx.store, |s| {
                s.sessions().linked_operations(&session.id, 10)
            })
            .into_iter()
            .map(|link| {
                let operation = read(&*fx.store, |s| {
                    s.operations()
                        .operation(link.operation_id.as_ref().unwrap())
                })
                .unwrap();
                (operation.capability, operation.state, operation.payload)
            })
            .collect();
            let responses = read(&*fx.store, |s| s.sessions().entries(&session.id, 0, 500))
                .into_iter()
                .filter(|entry| entry.kind == "ToolResponseRecorded")
                .map(|entry| entry.body)
                .collect();
            let turn = Turn {
                alias,
                bodies: wire.bodies.lock().unwrap().clone(),
                operations,
                responses,
                session_id: json!(session.id),
            };

            let _ = remote.cancel().await;
            Arc::try_unwrap(app)
                .ok()
                .expect("runtime has one owner")
                .shutdown()
                .await;
            turn
        })
}

/// The provider message carrying the result of `call_id`.
fn tool_result<'a>(body: &'a Value, call_id: &str) -> &'a Value {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == call_id)
        .unwrap_or_else(|| panic!("no tool result for {call_id}: {body}"))
}

#[test]
fn an_agent_turn_calls_a_selected_mcp_tool_and_reads_its_result_in_the_next_request() {
    let turn = agent_turn(vec![("call-mcp", "DW-45")]);

    // Two provider requests: the first offers the tool, the second carries its result.
    assert_eq!(turn.bodies.len(), 2, "{:?}", turn.bodies);
    let offered: Vec<_> = turn.bodies[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(offered.contains(&turn.alias), "{offered:?}");
    let result = tool_result(&turn.bodies[1], "call-mcp");
    assert!(result.to_string().contains(ANSWER), "{result}");

    // The call was journaled as one settled `mcp.tool` operation for this session.
    assert_eq!(turn.operations.len(), 1);
    let (capability, state, payload) = &turn.operations[0];
    assert_eq!(capability, "mcp.tool");
    assert_eq!(*state, OperationState::ObservedComplete);
    assert_eq!(payload["sessionId"], turn.session_id, "{payload:?}");
    assert_eq!(turn.responses.len(), 1);
    assert_eq!(turn.responses[0]["call"]["nativeCallId"], "call-mcp");
}

#[test]
fn a_failing_mcp_tool_call_does_not_block_an_independent_call_in_the_same_turn() {
    let turn = agent_turn(vec![("call-broken", "BROKEN"), ("call-mcp", "DW-45")]);

    // Both calls were answered in the same follow-up request: the failure as a failure, the
    // independent call with the server's result.
    assert_eq!(turn.bodies.len(), 2, "{:?}", turn.bodies);
    let answered = tool_result(&turn.bodies[1], "call-mcp");
    assert!(answered.to_string().contains(ANSWER), "{answered}");
    let broken = tool_result(&turn.bodies[1], "call-broken").to_string();
    assert!(!broken.contains("echo-server saw"), "{broken}");

    // Each call settled as its own `mcp.tool` operation; only the broken one failed.
    let states: Vec<_> = turn
        .operations
        .iter()
        .map(|(capability, state, _)| {
            assert_eq!(capability, "mcp.tool");
            *state
        })
        .collect();
    assert_eq!(states.len(), 2, "{:?}", turn.operations);
    assert!(
        states.contains(&OperationState::ObservedComplete),
        "{states:?}"
    );
    assert!(
        states
            .iter()
            .any(|state| *state != OperationState::ObservedComplete),
        "{states:?}"
    );
    let mut answered_calls: Vec<_> = turn
        .responses
        .iter()
        .map(|body| body["call"]["nativeCallId"].as_str().unwrap().to_owned())
        .collect();
    answered_calls.sort();
    assert_eq!(answered_calls, ["call-broken", "call-mcp"]);
}
