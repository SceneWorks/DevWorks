//! A selected in-process MCP server samples during an agent's dispatched tool call. The request
//! crosses the real native client, the boot-installed sampler and the router to scripted
//! provider bytes; no model runs.
use super::*;
use devworks_adapters::mcp::NativeMcpClients;
use devworks_core::authority::destructive::ProtectedRoots;
use devworks_core::ports::mcp::{McpEndpoint, RemoteCatalog, RemoteTool};
use devworks_core::protocol::results::{DomainDecision, OperationResult};
use devworks_core::protocol::{CommandReply, OperationState};
use devworks_core::services::mcp::McpClientService;
use std::sync::OnceLock;

/// Each tool call asks the client for one sample and returns what it received.
struct SamplingServer;
impl rmcp::ServerHandler for SamplingServer {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        rmcp::model::ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
    }

    #[allow(deprecated)]
    async fn call_tool(
        &self,
        _request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let sampled = context
            .peer
            .create_message(rmcp::model::CreateMessageRequestParams::new(
                vec![rmcp::model::SamplingMessage::user_text(
                    "Summarize the change",
                )],
                64,
            ))
            .await
            .map_err(|error| rmcp::ErrorData::internal_error(error.to_string(), None))?;
        let text = sampled
            .message
            .content
            .first()
            .and_then(|content| content.as_text())
            .map(|content| content.text.clone())
            .unwrap_or_default();
        Ok(rmcp::model::CallToolResponse::Complete(
            rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(format!(
                "server received: {text} ({})",
                sampled.stop_reason.unwrap_or_default()
            ))]),
        ))
    }
}

fn decision(
    app: &devworks_engine::Runtime,
    ctx: &InvocationContext,
    op: &str,
    mut body: Value,
) -> DomainDecision {
    body["kind"] = json!(op);
    let frame = json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":8,
        "requestKey":format!("e2e-{}", op.replace(['.', '_'], "-")),"body":body});
    match app.command(ctx, op, frame.to_string().as_bytes()) {
        Ok(CommandReply::Recorded { receipt, .. }) => receipt.decision,
        other => panic!("{op} was not recorded: {other:?}"),
    }
}

#[test]
fn a_selected_server_samples_through_the_boot_installed_sampler_during_an_agent_tool_call() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let f = Fx::new(
                "CompatibleChat",
                Script::sse(super::mcp_sampling::WIRE),
                false,
            );
            let (agent, _) = super::mcp_sampling::agent_session(&f);
            let human = principal(f.store.as_ref(), "mcp-owner", PrincipalKind::Human);
            let service = Arc::new(McpClientService::new(f.h.ids.clone()));
            let clients: Arc<OnceLock<Arc<NativeMcpClients>>> = Arc::default();
            let mut composition = tool_runtime_composition(&f);
            composition.use_cases = composition.use_cases.with_mcp(service.clone());
            let installed = clients.clone();
            composition.config.mcp = Some(devworks_engine::runtime::McpComposition {
                validator: Arc::new(devworks_adapters::packages::JsonSchemaValidator),
                factory: Arc::new(move |supervisor, handle| {
                    let native = Arc::new(NativeMcpClients::new(
                        supervisor,
                        Default::default(),
                        Arc::new(SessionSecrets::new()),
                        "mcp-e2e".into(),
                        Arc::new(NativeFilesystem::new()),
                        ProtectedRoots::new(vec![], vec![], None),
                        vec![],
                        handle,
                    ));
                    let _ = installed.set(native.clone());
                    native
                }),
            });
            let app = devworks_engine::boot(composition).await.unwrap();

            // A human configures the server, its catalog is published, and the human selects
            // both its tool and its sampling capability for agents.
            let transport = json!({"kind":"Http","endpoint":"http://127.0.0.1:9/mcp"});
            let DomainDecision::Applied {
                result: OperationResult::McpConfigure { value: server },
            } = decision(
                &app,
                &human,
                "mcp.configure",
                json!({"scope":{"kind":"Workspace"},
                "name":"Sampling server","transport":transport}),
            )
            else {
                panic!("server configuration failed");
            };
            let (catalog_service, server_id) = (service.clone(), server.id.clone());
            write(f.store.as_ref(), move |tx| {
                catalog_service.publish_remote_catalog(
                    tx,
                    &ConfigScope::Workspace,
                    &server_id,
                    &Revision::new(1).unwrap(),
                    RemoteCatalog {
                        tools: vec![RemoteTool {
                            name: "summarize".into(),
                            description: "Summarize through the client".into(),
                            input_schema: json!({"type":"object"}),
                            output_schema: None,
                            read_only_hint: false,
                        }],
                        resources: vec![],
                        prompts: vec![],
                    },
                )
            })
            .unwrap();
            let (tool, sampling) = read(f.store.as_ref(), |s| {
                let state = s
                    .configuration()
                    .current(&ConfigScope::Workspace, "outgoing_mcp")?
                    .unwrap()
                    .body;
                let caps = state["servers"][0]["capabilities"]
                    .as_array()
                    .unwrap()
                    .clone();
                let find = |kind: &str| {
                    caps.iter()
                        .find(|cap| cap["kind"] == kind)
                        .map(|cap| cap["id"].as_str().unwrap().to_owned())
                        .unwrap()
                };
                Ok((find("tool"), find("sampling")))
            });
            let DomainDecision::Applied { .. } = decision(
                &app,
                &human,
                "mcp.select_tools",
                json!({"scope":{"kind":"Workspace"},"serverId":server.id,"expectedRevision":"2",
                    "expectedCatalogRevision":"2","toolIds":[tool, sampling]}),
            ) else {
                panic!("selection failed");
            };
            let capability = format!("mcp/{}", tool.strip_prefix("mcp-").unwrap());
            write(f.store.as_ref(), move |tx| {
                let current = devworks_core::settings::model::current_layer(
                    tx.configuration(),
                    &ConfigScope::Workspace,
                )?;
                let descriptor = devworks_core::settings::catalog::descriptor(
                    devworks_core::settings::catalog::TOOL_OVERRIDES_KEY,
                )
                .unwrap();
                let assignment = serde_json::from_value(json!({"key": descriptor.key,
                    "value": {"schema": descriptor.schema_ref(), "value": {(capability): true}}}))
                .unwrap();
                let plan = devworks_core::settings::model::plan(&current, &[assignment]);
                let body = devworks_core::settings::model::body(
                    &plan,
                    &ActorId(id("mcp-owner")),
                    tx.now_ms(),
                    json!({"kind": "test"}),
                );
                tx.configuration().append_revision(
                    &ConfigScope::Workspace,
                    devworks_core::settings::model::NAMESPACE,
                    current.revision,
                    devworks_core::settings::model::BODY_SCHEMA_VERSION,
                    body,
                )
            })
            .unwrap();

            // The in-process server takes the place of the endpoint's connection.
            let (client_reader, server_writer) = tokio::io::duplex(64 * 1024);
            let (server_reader, client_writer) = tokio::io::duplex(64 * 1024);
            let remote = tokio::spawn(async move {
                rmcp::serve_server(SamplingServer, (server_reader, server_writer))
                    .await
                    .expect("in-process server handshake")
            });
            clients
                .get()
                .expect("boot installed the native MCP client")
                .attach_pipes(
                    &McpEndpoint {
                        server_id: server.id.clone(),
                        scope: ConfigScope::Workspace,
                        transport: serde_json::from_value(transport).unwrap(),
                        sampling: true,
                    },
                    client_reader,
                    client_writer,
                )
                .await
                .expect("attached server");
            let remote = remote.await.unwrap();
            assert!(
                remote
                    .peer()
                    .peer_info()
                    .unwrap()
                    .capabilities
                    .sampling
                    .is_some()
            );

            // The agent's selected tool call is journaled and dispatched by the runtime.
            let alias = read(f.store.as_ref(), |s| {
                service.selected_tool_materials(&agent, s)
            })
            .into_iter()
            .find(|material| material.wire_name == "summarize")
            .expect("selected agent tool")
            .alias;
            let call = ToolCallKey {
                attempt_id: AttemptId(id("provider-attempt")),
                native_call_id: "call-e2e".into(),
                ordinal: Counter::new(0).unwrap(),
            };
            let operation = service
                .queue_selected_tool(&agent, &call, &alias, json!({}))
                .unwrap();
            let settled = within("dispatched MCP tool call", async {
                loop {
                    let current =
                        read(f.store.as_ref(), |s| s.operations().operation(&operation)).unwrap();
                    if !matches!(
                        current.state,
                        OperationState::IntentRecorded
                            | OperationState::Preflighted
                            | OperationState::Dispatching
                    ) {
                        break current;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await;
            assert_eq!(
                settled.state,
                OperationState::ObservedComplete,
                "{settled:?}"
            );
            let observed =
                serde_json::to_string(&settled.latest_attempt().unwrap().observations).unwrap();
            assert!(
                observed.contains("server received: Sampled answer (endTurn)"),
                "{observed}"
            );
            assert_eq!(f.http.lock().unwrap().requests.lock().unwrap().len(), 1);
            let categories = read(f.store.as_ref(), |s| {
                s.usage()
                    .inference_attempts(&UsageAttemptFilter::default(), None, 64)?
                    .into_iter()
                    .map(|attempt| {
                        Ok(s.operations()
                            .operation(&attempt.operation_id)?
                            .unwrap()
                            .payload["reportCategory"]
                            .clone())
                    })
                    .collect::<Result<Vec<_>, StorageError>>()
            });
            assert_eq!(categories, [json!("Mcp")]);
            assert!(
                read(f.store.as_ref(), |s| s.sessions().entries(
                    &id("agent-session"),
                    0,
                    500
                ))
                .is_empty()
            );
            let _ = remote.cancel().await;
            app.shutdown().await;
            // The installed sampler does not keep the MCP service, its native client or the
            // store alive after the runtime is gone.
            let (weak_service, native) = (
                Arc::downgrade(&service),
                Arc::downgrade(clients.get().unwrap()),
            );
            drop((app, clients, service));
            assert!(weak_service.upgrade().is_none(), "MCP service retained");
            assert!(native.upgrade().is_none(), "native MCP client retained");
        });
}
