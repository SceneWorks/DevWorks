//! Outgoing MCP wire sessions. Configuration and admission belong to the core service; this
//! adapter owns protocol framing and uses the existing process supervisor for local servers.
use devworks_core::authority::destructive::{FilesystemResolver, ProtectedRoots};
use devworks_core::context::InvocationContext;
use devworks_core::ports::mcp::{
    McpEndpoint, McpRemote, McpRemoteFault, RemoteCatalog, RemotePrompt, RemoteResource, RemoteTool,
};
use devworks_core::ports::process::ProcessSpec;
use devworks_core::ports::secrets::{Secret, SecretKey, SecretStore};
use devworks_core::protocol::Id;
use devworks_core::protocol::records::McpTransport;
use devworks_core::services::process::tools::{BindingRefusal, ToolBindings};
use devworks_core::services::process::{
    LaunchPolicy, LaunchRequest, OwnedProcess, ProcessSupervisor,
};
use devworks_core::services::router::connections::auth::Credential;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ClientConfig, GetPromptRequestParams,
    GetPromptResponse, PaginatedRequestParams, Prompt, ReadResourceRequestParams,
    ReadResourceResponse, Resource, Tool,
};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::{RoleClient, serve_client};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A connected wire session. The owner is retained so callers can stop an app-launched server
/// through the same journaled process path that started it.
pub struct McpWireSession {
    client: RunningService<RoleClient, ClientConfig>,
    process: Option<OwnedProcessGuard>,
}

struct ConnectedSession {
    key: Vec<u8>,
    wire: McpWireSession,
}
type SessionSlot = Arc<tokio::sync::Mutex<Option<ConnectedSession>>>;

struct OwnedProcessGuard {
    process: OwnedProcess,
    supervisor: Arc<ProcessSupervisor>,
}
impl Drop for OwnedProcessGuard {
    fn drop(&mut self) {
        // Also covers callers that discard a failed or interrupted connection without close().
        let _ = self.supervisor.cancel(
            &self.process,
            Duration::from_secs(1),
            Duration::from_secs(5),
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectFault {
    InvalidEndpoint,
    LaunchRefused,
    MissingPipe,
    Handshake,
    Deadline,
    Protocol,
    TooLarge,
    StopFailed,
    InputRequired,
}

const CONNECT_DEADLINE: Duration = Duration::from_secs(20);
const CALL_DEADLINE: Duration = Duration::from_secs(15);
const CATALOG_MAX: usize = 500;
const MCP_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

pub struct McpWireCatalog {
    pub tools: Vec<Tool>,
    pub resources: Vec<Resource>,
    pub prompts: Vec<Prompt>,
}

/// Native outgoing connection owner. Each server has its own async lock, so one unhealthy
/// server cannot serialize unrelated discovery or connection work.
pub struct NativeMcpClients {
    supervisor: Arc<ProcessSupervisor>,
    bindings: ToolBindings,
    secrets: Arc<dyn SecretStore>,
    secret_namespace: String,
    filesystem: Arc<dyn FilesystemResolver>,
    protected_roots: ProtectedRoots,
    protected_branches: Vec<String>,
    runtime: tokio::runtime::Handle,
    sessions: Mutex<HashMap<String, SessionSlot>>,
}
impl NativeMcpClients {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        supervisor: Arc<ProcessSupervisor>,
        bindings: ToolBindings,
        secrets: Arc<dyn SecretStore>,
        secret_namespace: String,
        filesystem: Arc<dyn FilesystemResolver>,
        protected_roots: ProtectedRoots,
        protected_branches: Vec<String>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            supervisor,
            bindings,
            secrets,
            secret_namespace,
            filesystem,
            protected_roots,
            protected_branches,
            runtime,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn connect_wire(
        &self,
        context: &InvocationContext,
        endpoint: &McpEndpoint,
    ) -> Result<McpWireSession, McpRemoteFault> {
        match &endpoint.transport {
            McpTransport::Http {
                endpoint,
                credential_ref,
            } => {
                if credential_ref
                    .as_ref()
                    .is_some_and(|handle| !handle.as_str().starts_with("mcp-"))
                {
                    return Err(McpRemoteFault::InvalidConfiguration);
                }
                let secret = credential_ref
                    .as_ref()
                    .map(|id| {
                        self.secrets
                            .get(&SecretKey::new(&self.secret_namespace, id.clone()))
                            .ok()
                            .flatten()
                            .ok_or(McpRemoteFault::MissingCredential)
                    })
                    .transpose()?;
                let bearer = mcp_bearer(secret.as_ref())?;
                self.runtime
                    .block_on(McpWireSession::http(
                        &endpoint.0,
                        bearer.as_ref().map(|s| s.expose()),
                    ))
                    .map_err(map_connect_fault)
            }
            McpTransport::Stdio { process } => {
                let mut native = self
                    .bindings
                    .resolve(process, endpoint.scope.project_id())
                    .map_err(map_binding_fault)?;
                native.env = self
                    .bindings
                    .resolve_container_environment(
                        &process.environment.0,
                        endpoint.scope.project_id(),
                        self.secrets.as_ref(),
                        &self.secret_namespace,
                    )
                    .map_err(map_binding_fault)?;
                let policy = LaunchPolicy {
                    fs: self.filesystem.as_ref(),
                    roots: &self.protected_roots,
                    protected_branches: &self.protected_branches,
                };
                self.runtime
                    .block_on(McpWireSession::stdio(
                        self.supervisor.clone(),
                        context,
                        native,
                        &policy,
                    ))
                    .map_err(map_connect_fault)
            }
        }
    }
}

fn map_connect_fault(fault: ConnectFault) -> McpRemoteFault {
    match fault {
        ConnectFault::InvalidEndpoint => McpRemoteFault::InvalidConfiguration,
        ConnectFault::LaunchRefused => McpRemoteFault::LaunchRefused,
        ConnectFault::MissingPipe | ConnectFault::StopFailed => McpRemoteFault::UnavailableRuntime,
        ConnectFault::Handshake | ConnectFault::Protocol => McpRemoteFault::Protocol,
        ConnectFault::Deadline => McpRemoteFault::Deadline,
        ConnectFault::TooLarge => McpRemoteFault::TooLarge,
        ConnectFault::InputRequired => McpRemoteFault::InputRequired,
    }
}
fn map_binding_fault(fault: BindingRefusal) -> McpRemoteFault {
    match fault {
        BindingRefusal::UnknownExecutable | BindingRefusal::ChangedExecutable => {
            McpRemoteFault::UnavailableRuntime
        }
        BindingRefusal::UnknownCredential => McpRemoteFault::MissingCredential,
        _ => McpRemoteFault::InvalidConfiguration,
    }
}

fn bounded_result(result: &impl serde::Serialize) -> Result<serde_json::Value, ConnectFault> {
    let bytes = serde_json::to_vec(result).map_err(|_| ConnectFault::Protocol)?;
    if bytes.len() > MCP_MAX_FRAME_BYTES {
        return Err(ConnectFault::TooLarge);
    }
    serde_json::from_slice(&bytes).map_err(|_| ConnectFault::Protocol)
}

fn mcp_bearer(stored: Option<&Secret>) -> Result<Option<Secret>, McpRemoteFault> {
    stored
        .map(|value| match Credential::parse(value) {
            Some(Credential::ApiKey(key)) => Ok(key),
            _ => Err(McpRemoteFault::MissingCredential),
        })
        .transpose()
}

impl McpRemote for NativeMcpClients {
    fn connect(
        &self,
        context: &InvocationContext,
        endpoint: &McpEndpoint,
    ) -> Result<(), McpRemoteFault> {
        let key = serde_json::to_vec(&(endpoint.scope.key(), &endpoint.transport))
            .map_err(|_| McpRemoteFault::InvalidConfiguration)?;
        let slot = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(endpoint.server_id.as_str().into())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
            .clone();
        // Keep the same slot across reconnects. Concurrent callers either reuse this endpoint
        // or wait for the changed endpoint to finish replacing its session.
        let mut guard = self.runtime.block_on(slot.lock());
        if guard
            .as_ref()
            .is_some_and(|connected| connected.key == key && !connected.wire.client.is_closed())
        {
            return Ok(());
        }
        if let Some(old) = guard.take()
            && !old.wire.client.is_closed()
        {
            self.runtime
                .block_on(old.wire.close())
                .map_err(map_connect_fault)?;
        }
        let session = self.connect_wire(context, endpoint)?;
        *guard = Some(ConnectedSession { key, wire: session });
        Ok(())
    }

    fn discover(&self, server_id: &Id) -> Result<RemoteCatalog, McpRemoteFault> {
        let slot = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(server_id.as_str())
            .cloned()
            .ok_or(McpRemoteFault::Unreachable)?;
        let catalog = self
            .runtime
            .block_on(async {
                let guard = slot.lock().await;
                guard
                    .as_ref()
                    .ok_or(ConnectFault::Protocol)?
                    .wire
                    .discover()
                    .await
            })
            .map_err(map_connect_fault)?;
        Ok(RemoteCatalog {
            tools: catalog
                .tools
                .into_iter()
                .map(|tool| RemoteTool {
                    name: tool.name.into_owned(),
                    description: tool.description.map(|s| s.into_owned()).unwrap_or_default(),
                    input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
                    output_schema: tool
                        .output_schema
                        .map(|s| serde_json::Value::Object((*s).clone())),
                    read_only_hint: tool
                        .annotations
                        .and_then(|a| a.read_only_hint)
                        .unwrap_or(false),
                })
                .collect(),
            resources: catalog
                .resources
                .into_iter()
                .map(|r| RemoteResource {
                    uri: r.uri,
                    name: r.name,
                    description: r.description.unwrap_or_default(),
                })
                .collect(),
            prompts: catalog
                .prompts
                .into_iter()
                .map(|p| RemotePrompt {
                    name: p.name,
                    description: p.description.unwrap_or_default(),
                    arguments: p
                        .arguments
                        .unwrap_or_default()
                        .into_iter()
                        .map(|argument| devworks_core::ports::mcp::RemotePromptArgument {
                            name: argument.name,
                            required: argument.required.unwrap_or(false),
                        })
                        .collect(),
                })
                .collect(),
        })
    }

    fn call_tool(
        &self,
        server_id: &Id,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpRemoteFault> {
        let serde_json::Value::Object(arguments) = arguments else {
            return Err(McpRemoteFault::InvalidConfiguration);
        };
        let slot = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(server_id.as_str())
            .cloned()
            .ok_or(McpRemoteFault::Unreachable)?;
        self.runtime.block_on(async {
            let guard = slot.lock().await;
            let session = guard.as_ref().ok_or(McpRemoteFault::Unreachable)?;
            session
                .wire
                .call_tool(name, arguments)
                .await
                .map_err(map_connect_fault)
        })
    }

    fn read_resource(
        &self,
        server_id: &Id,
        uri: &str,
    ) -> Result<serde_json::Value, McpRemoteFault> {
        let slot = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(server_id.as_str())
            .cloned()
            .ok_or(McpRemoteFault::Unreachable)?;
        self.runtime.block_on(async {
            let guard = slot.lock().await;
            let session = guard.as_ref().ok_or(McpRemoteFault::Unreachable)?;
            session
                .wire
                .read_resource(uri)
                .await
                .map_err(map_connect_fault)
        })
    }

    fn get_prompt(
        &self,
        server_id: &Id,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpRemoteFault> {
        let serde_json::Value::Object(arguments) = arguments else {
            return Err(McpRemoteFault::InvalidConfiguration);
        };
        let slot = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(server_id.as_str())
            .cloned()
            .ok_or(McpRemoteFault::Unreachable)?;
        self.runtime.block_on(async {
            let guard = slot.lock().await;
            let session = guard.as_ref().ok_or(McpRemoteFault::Unreachable)?;
            session
                .wire
                .get_prompt(name, arguments)
                .await
                .map_err(map_connect_fault)
        })
    }

    fn disconnect(&self, server_id: &Id) -> Result<(), McpRemoteFault> {
        let slot = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(server_id.as_str())
            .cloned();
        if let Some(slot) = slot {
            self.runtime.block_on(async {
                if let Some(session) = slot.lock().await.take() {
                    session.wire.close().await.map_err(map_connect_fault)?;
                }
                Ok(())
            })
        } else {
            Ok(())
        }
    }
}

impl McpWireSession {
    pub async fn read_resource(&self, uri: &str) -> Result<serde_json::Value, ConnectFault> {
        let response = tokio::time::timeout(
            CALL_DEADLINE,
            self.client
                .read_resource_once(ReadResourceRequestParams::new(uri)),
        )
        .await
        .map_err(|_| ConnectFault::Deadline)?
        .map_err(|_| ConnectFault::Protocol)?;
        match response {
            ReadResourceResponse::Complete(result) => bounded_result(&result),
            ReadResourceResponse::InputRequired(_) => Err(ConnectFault::InputRequired),
            _ => Err(ConnectFault::Protocol),
        }
    }

    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, ConnectFault> {
        let params = GetPromptRequestParams::new(name).with_arguments(arguments);
        let response = tokio::time::timeout(CALL_DEADLINE, self.client.get_prompt_once(params))
            .await
            .map_err(|_| ConnectFault::Deadline)?
            .map_err(|_| ConnectFault::Protocol)?;
        match response {
            GetPromptResponse::Complete(result) => bounded_result(&result),
            GetPromptResponse::InputRequired(_) => Err(ConnectFault::InputRequired),
            _ => Err(ConnectFault::Protocol),
        }
    }

    /// Return the exact bounded protocol result. Sampling and elicitation are not advertised by
    /// the current client handler; servers that require them receive an explicit protocol fault.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, ConnectFault> {
        let params = CallToolRequestParams::new(name.to_owned()).with_arguments(arguments);
        let response = tokio::time::timeout(CALL_DEADLINE, self.client.call_tool_once(params))
            .await
            .map_err(|_| ConnectFault::Deadline)?
            .map_err(|_| ConnectFault::Protocol)?;
        let result = match response {
            CallToolResponse::Complete(result) => result,
            CallToolResponse::InputRequired(_) => return Err(ConnectFault::InputRequired),
            CallToolResponse::Task(_) => return Err(ConnectFault::Protocol),
            _ => return Err(ConnectFault::Protocol),
        };
        bounded_result(&result)
    }
    /// Read a bounded complete catalog. Each advertised capability is queried independently;
    /// an unsupported family stays empty, while a failed advertised family fails discovery.
    pub async fn discover(&self) -> Result<McpWireCatalog, ConnectFault> {
        let peer = self.client.peer_info().ok_or(ConnectFault::Protocol)?;
        tokio::time::timeout(CALL_DEADLINE, async {
            let mut tools = Vec::new();
            if peer.capabilities.tools.is_some() {
                let mut cursor = None;
                let mut seen = std::collections::HashSet::new();
                loop {
                    let page = self
                        .client
                        .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                        .await
                        .map_err(|_| ConnectFault::Protocol)?;
                    tools.extend(page.tools);
                    if tools.len() > CATALOG_MAX {
                        return Err(ConnectFault::TooLarge);
                    }
                    cursor = page.next_cursor;
                    let Some(next) = &cursor else {
                        break;
                    };
                    if !seen.insert(next.clone()) {
                        return Err(ConnectFault::Protocol);
                    }
                }
            }
            let mut resources = Vec::new();
            if peer.capabilities.resources.is_some() {
                let mut cursor = None;
                let mut seen = std::collections::HashSet::new();
                loop {
                    let page = self
                        .client
                        .list_resources(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                        .await
                        .map_err(|_| ConnectFault::Protocol)?;
                    resources.extend(page.resources);
                    if resources.len() > CATALOG_MAX {
                        return Err(ConnectFault::TooLarge);
                    }
                    cursor = page.next_cursor;
                    let Some(next) = &cursor else {
                        break;
                    };
                    if !seen.insert(next.clone()) {
                        return Err(ConnectFault::Protocol);
                    }
                }
            }
            let mut prompts = Vec::new();
            if peer.capabilities.prompts.is_some() {
                let mut cursor = None;
                let mut seen = std::collections::HashSet::new();
                loop {
                    let page = self
                        .client
                        .list_prompts(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                        .await
                        .map_err(|_| ConnectFault::Protocol)?;
                    prompts.extend(page.prompts);
                    if prompts.len() > CATALOG_MAX {
                        return Err(ConnectFault::TooLarge);
                    }
                    cursor = page.next_cursor;
                    let Some(next) = &cursor else {
                        break;
                    };
                    if !seen.insert(next.clone()) {
                        return Err(ConnectFault::Protocol);
                    }
                }
            }
            Ok(McpWireCatalog {
                tools,
                resources,
                prompts,
            })
        })
        .await
        .map_err(|_| ConnectFault::Deadline)?
    }
    /// Connect to a remote Streamable HTTP endpoint. A credential is resolved by the host from
    /// its secret store and passed only for the lifetime of this session.
    pub async fn http(endpoint: &str, bearer: Option<&str>) -> Result<Self, ConnectFault> {
        let url = url::Url::parse(endpoint).map_err(|_| ConnectFault::InvalidEndpoint)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(ConnectFault::InvalidEndpoint);
        }
        // Sending credentials over plain HTTP is allowed only to an actual loopback address;
        // a DNS name that currently resolves locally is not a stable security boundary.
        if bearer.is_some()
            && url.scheme() == "http"
            && !matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))
        {
            return Err(ConnectFault::InvalidEndpoint);
        }
        let mut config =
            rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                endpoint.to_owned(),
            )
            .max_sse_event_size(MCP_MAX_FRAME_BYTES);
        // rmcp's reqwest client also bounds JSON response bodies to 16 MiB before decoding.
        config.auth_header = bearer.map(|token| format!("Bearer {token}"));
        let transport = StreamableHttpClientTransport::from_config(config);
        let client = tokio::time::timeout(
            CONNECT_DEADLINE,
            serve_client(ClientConfig::default(), transport),
        )
        .await
        .map_err(|_| ConnectFault::Deadline)?
        .map_err(|_| ConnectFault::Handshake)?;
        Ok(Self {
            client,
            process: None,
        })
    }

    /// Launch a local server through the owned process supervisor and attach the SDK to its
    /// stdio pipes. The SDK's child-process transport must not spawn a parallel unowned tree.
    pub async fn stdio(
        supervisor: Arc<ProcessSupervisor>,
        context: &InvocationContext,
        mut spec: ProcessSpec,
        policy: &LaunchPolicy<'_>,
    ) -> Result<Self, ConnectFault> {
        spec.capture_output = true;
        spec.merge_output = false;
        spec.pipe_stdin = true;
        spec.stdin = None;
        let process = supervisor
            .launch(
                context,
                LaunchRequest {
                    spec,
                    operation_id: None,
                    lease: None,
                    approved_hold: false,
                },
                policy,
            )
            .map_err(|_| ConnectFault::LaunchRefused)?;
        let guard = OwnedProcessGuard {
            process,
            supervisor,
        };
        let stdin = guard
            .supervisor
            .take_input(&guard.process)
            .ok_or(ConnectFault::MissingPipe)?;
        let (stdout, stderr) = guard
            .supervisor
            .take_output(&guard.process)
            .ok_or(ConnectFault::MissingPipe)?;
        let (reader, mut stdout_tx) = tokio::io::duplex(64 * 1024);
        let (mut stdin_rx, writer) = tokio::io::duplex(64 * 1024);
        let runtime = tokio::runtime::Handle::current();
        let read_runtime = runtime.clone();
        tokio::task::spawn_blocking(move || {
            let mut stdout = stdout;
            let mut buffer = [0; 16 * 1024];
            while let Ok(count) = stdout.read(&mut buffer) {
                if count == 0
                    || read_runtime
                        .block_on(stdout_tx.write_all(&buffer[..count]))
                        .is_err()
                {
                    break;
                }
            }
        });
        tokio::task::spawn_blocking(move || {
            let mut stdin = stdin;
            let mut buffer = [0; 16 * 1024];
            while let Ok(count) = runtime.block_on(stdin_rx.read(&mut buffer)) {
                if count == 0 || stdin.write_all(&buffer[..count]).is_err() {
                    break;
                }
            }
        });
        // Stderr cannot share stdout: the JSON-RPC stream must stay parseable. Drain it so a
        // verbose server cannot block on a full pipe; diagnostics are owned by the caller.
        tokio::task::spawn_blocking(move || {
            let mut stderr = stderr;
            let mut buffer = [0; 4096];
            while matches!(stderr.read(&mut buffer), Ok(n) if n > 0) {}
        });
        let client = connect_pipes(reader, writer).await?;
        Ok(Self {
            client,
            process: Some(guard),
        })
    }

    /// Close the protocol session and settle its owned process tree within a fixed bound.
    pub async fn close(self) -> Result<(), ConnectFault> {
        let Self { client, process } = self;
        let _ = client.cancel().await;
        if let Some(process) = process {
            tokio::task::spawn_blocking(move || drop(process))
                .await
                .map_err(|_| ConnectFault::StopFailed)?;
        }
        Ok(())
    }
}

async fn connect_pipes(
    reader: tokio::io::DuplexStream,
    writer: tokio::io::DuplexStream,
) -> Result<RunningService<RoleClient, ClientConfig>, ConnectFault> {
    tokio::time::timeout(
        CONNECT_DEADLINE,
        serve_client(
            ClientConfig::default(),
            AsyncRwTransport::new(reader, writer).with_max_line_length(MCP_MAX_FRAME_BYTES),
        ),
    )
    .await
    .map_err(|_| ConnectFault::Deadline)?
    .map_err(|_| ConnectFault::Handshake)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_bearer_uses_only_the_key_from_a_staged_credential() {
        let stored = Credential::ApiKey(Secret::new("mcp-test-token")).to_secret();
        let bearer = mcp_bearer(Some(&stored)).unwrap().unwrap();
        assert_eq!(bearer.expose(), "mcp-test-token");
        assert!(mcp_bearer(None).unwrap().is_none());
        assert_eq!(
            mcp_bearer(Some(&Secret::new("unrecognized secret"))).err(),
            Some(McpRemoteFault::MissingCredential)
        );
    }

    struct EmptyServer;
    impl rmcp::ServerHandler for EmptyServer {
        fn get_info(&self) -> rmcp::model::ServerConfig {
            rmcp::model::ServerConfig::new(
                rmcp::model::ServerCapabilities::builder()
                    .enable_tools()
                    .build(),
            )
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> Result<rmcp::model::ListToolsResult, rmcp::ErrorData> {
            let mut result = rmcp::model::ListToolsResult::default();
            let mut tool = Tool::default();
            tool.name = "read_note".into();
            result.tools.push(tool);
            Ok(result)
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> Result<CallToolResponse, rmcp::ErrorData> {
            assert_eq!(request.name, "read_note");
            assert_eq!(
                request.arguments.as_ref().and_then(|v| v.get("id")),
                Some(&serde_json::json!("note-1"))
            );
            Ok(CallToolResponse::Complete(
                rmcp::model::CallToolResult::success(vec![]),
            ))
        }
    }

    #[tokio::test]
    async fn owned_pipe_adapter_completes_a_native_mcp_handshake() {
        let (client_reader, server_writer) = tokio::io::duplex(64 * 1024);
        let (server_reader, client_writer) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            rmcp::serve_server(EmptyServer, (server_reader, server_writer))
                .await
                .expect("local server handshake")
        });
        let client = connect_pipes(client_reader, client_writer)
            .await
            .expect("native client handshake");
        assert!(client.peer_info().is_some());
        let session = McpWireSession {
            client,
            process: None,
        };
        let catalog = session.discover().await.expect("discovery");
        assert_eq!(catalog.tools.len(), 1);
        assert_eq!(catalog.tools[0].name, "read_note");
        assert!(catalog.resources.is_empty());
        assert!(catalog.prompts.is_empty());
        let result = session
            .call_tool(
                "read_note",
                serde_json::Map::from_iter([("id".into(), serde_json::json!("note-1"))]),
            )
            .await
            .expect("native tool call");
        assert_eq!(result["isError"], false);
        let _ = session.close().await;
        server.abort();
    }

    #[tokio::test]
    async fn credential_cannot_be_sent_to_cleartext_remote_or_url_userinfo() {
        assert_eq!(
            McpWireSession::http("http://example.test/mcp", Some("secret"))
                .await
                .err(),
            Some(ConnectFault::InvalidEndpoint)
        );
        assert_eq!(
            McpWireSession::http("https://user:pass@example.test/mcp", None)
                .await
                .err(),
            Some(ConnectFault::InvalidEndpoint)
        );
    }

    #[tokio::test]
    async fn unavailable_local_server_reports_failed_handshake_without_a_session() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("local listener");
        let endpoint = format!("http://{}/mcp", listener.local_addr().expect("address"));
        drop(listener);
        assert_eq!(
            McpWireSession::http(&endpoint, None).await.err(),
            Some(ConnectFault::Handshake)
        );
    }
}
