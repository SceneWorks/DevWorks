//! Outgoing MCP wire sessions. Configuration and admission belong to the core service; this
//! adapter owns protocol framing and uses the existing process supervisor for local servers.
use devworks_core::authority::destructive::{FilesystemResolver, ProtectedRoots};
use devworks_core::context::InvocationContext;
use devworks_core::ports::mcp::{
    McpCallOrigin, McpEndpoint, McpRemote, McpRemoteFault, McpSampler, RemoteCatalog, RemotePrompt,
    RemoteResource, RemoteTool, SamplingCall, SamplingMessage, SamplingRefusal,
};
use devworks_core::ports::process::ProcessSpec;
use devworks_core::ports::secrets::{Secret, SecretKey, SecretStore};
use devworks_core::ports::storage::Cancellation;
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
use rmcp::service::{RequestContext, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::{RoleClient, serve_client};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A connected wire session. The owner is retained so callers can stop an app-launched server
/// through the same journaled process path that started it.
pub struct McpWireSession {
    client: RunningService<RoleClient, SamplingClient>,
    process: Option<OwnedProcessGuard>,
}

type SharedSampler = Arc<OnceLock<Arc<dyn McpSampler>>>;

/// The client side of one connection. Sampling is advertised only for a selected server and
/// accepted only while this client has an attributed tool call outstanding.
pub struct SamplingClient {
    sampling: bool,
    sampler: SharedSampler,
    in_flight: Mutex<Option<InFlight>>,
}

struct InFlight {
    origin: McpCallOrigin,
    samples: u8,
}

/// Clears the attributed call when its request settles or its future is dropped.
struct InFlightGuard<'a>(&'a SamplingClient);
impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        *self.0.in_flight.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// With four samples a call can last at most 15 s + 4 x 60 s.
const MAX_SAMPLES_PER_CALL: u8 = 4;
const SAMPLE_BUDGET: Duration = Duration::from_secs(60);
const MAX_SAMPLE_MESSAGES: usize = 64;
const MAX_SAMPLE_TEXT_BYTES: usize = 256 * 1024;
const MAX_SYSTEM_PROMPT_BYTES: usize = 32 * 1024;
const MAX_SAMPLE_TOKENS: u32 = 4096;
const MAX_RESULT_TEXT_BYTES: usize = 1024 * 1024;

impl SamplingClient {
    fn new(sampling: bool, sampler: SharedSampler) -> Self {
        Self {
            sampling,
            sampler,
            in_flight: Mutex::new(None),
        }
    }

    fn begin(&self, origin: Option<McpCallOrigin>) -> InFlightGuard<'_> {
        *self.in_flight.lock().unwrap_or_else(|e| e.into_inner()) =
            origin.map(|origin| InFlight { origin, samples: 0 });
        InFlightGuard(self)
    }

    /// Each accepted sample extends its call by one sample budget, up to a fixed total.
    fn call_deadline(&self, started: Instant) -> Instant {
        let samples = self
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(0, |call| u32::from(call.samples));
        started + CALL_DEADLINE + SAMPLE_BUDGET * samples
    }

    /// Reserve the next sample of the attributed call, if one is outstanding.
    fn admit(&self, reserve: bool) -> Result<(McpCallOrigin, u8), rmcp::ErrorData> {
        let mut guard = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        let call = guard.as_mut().ok_or_else(|| {
            rmcp::ErrorData::invalid_request(
                "sampling is accepted only during a DevWorks tool call",
                None,
            )
        })?;
        if !reserve {
            return Ok((call.origin.clone(), call.samples));
        }
        if call.samples >= MAX_SAMPLES_PER_CALL {
            return Err(rmcp::ErrorData::invalid_request(
                "sampling request limit for this tool call reached",
                None,
            ));
        }
        call.samples += 1;
        Ok((call.origin.clone(), call.samples))
    }
}

/// Map a server request to the text-only request DevWorks routes. Model preferences and
/// metadata are ignored: the router, not the server, chooses the model.
#[allow(deprecated)]
fn sampling_call(
    params: rmcp::model::CreateMessageRequestParams,
) -> Result<SamplingCall, &'static str> {
    use rmcp::model::{ContextInclusion, Role, SamplingMessageContentBlock};
    if !matches!(params.include_context, None | Some(ContextInclusion::None)) {
        return Err("includeContext is not supported");
    }
    if params.tools.is_some() || params.tool_choice.is_some() {
        return Err("sampling tools are not supported");
    }
    if params.messages.is_empty() || params.messages.len() > MAX_SAMPLE_MESSAGES {
        return Err("sampling needs 1 to 64 messages");
    }
    if params.messages.last().is_some_and(|m| m.role != Role::User) {
        return Err("the last sampling message must be from the user");
    }
    if params.max_tokens == 0 || params.max_tokens > MAX_SAMPLE_TOKENS {
        return Err("maxTokens must be between 1 and 4096");
    }
    let temperature = params.temperature.map(f64::from);
    if temperature.is_some_and(|t| !t.is_finite() || !(0.0..=1.0).contains(&t)) {
        return Err("temperature must be between 0 and 1");
    }
    let stop_sequences = params.stop_sequences.unwrap_or_default();
    if stop_sequences.len() > 4
        || stop_sequences
            .iter()
            .any(|stop| stop.is_empty() || stop.len() > 64)
    {
        return Err("stopSequences allows up to 4 sequences of 1 to 64 bytes");
    }
    if params
        .system_prompt
        .as_ref()
        .is_some_and(|prompt| prompt.len() > MAX_SYSTEM_PROMPT_BYTES)
    {
        return Err("systemPrompt exceeds 32 KiB");
    }
    let mut total = 0usize;
    let mut messages = Vec::with_capacity(params.messages.len());
    for message in params.messages {
        let mut text = String::new();
        for block in message.content.into_vec() {
            let SamplingMessageContentBlock::Text(block) = block else {
                return Err("only text sampling content is supported");
            };
            text.push_str(&block.text);
        }
        total = total.saturating_add(text.len());
        if text.is_empty() || total > MAX_SAMPLE_TEXT_BYTES {
            return Err("sampling text must be non-empty and at most 256 KiB");
        }
        messages.push(SamplingMessage {
            assistant: message.role == Role::Assistant,
            text,
        });
    }
    Ok(SamplingCall {
        ordinal: 0,
        messages,
        system_prompt: params.system_prompt,
        max_tokens: params.max_tokens,
        temperature,
        stop_sequences,
    })
}

#[allow(deprecated)]
impl rmcp::ClientHandler for SamplingClient {
    fn get_info(&self) -> ClientConfig {
        let mut config = ClientConfig::default();
        if self.sampling {
            config.capabilities.sampling = Some(rmcp::model::SamplingCapability::default());
        }
        config
    }

    async fn create_message(
        &self,
        params: rmcp::model::CreateMessageRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<rmcp::model::CreateMessageResult, rmcp::ErrorData> {
        use rmcp::model::{CreateMessageResult, SamplingMessage as WireMessage};
        if !self.sampling {
            return Err(rmcp::ErrorData::method_not_found::<
                rmcp::model::CreateMessageRequestMethod,
            >());
        }
        self.admit(false)?;
        let mut call = sampling_call(params)
            .map_err(|message| rmcp::ErrorData::invalid_params(message, None))?;
        let (origin, ordinal) = self.admit(true)?;
        call.ordinal = ordinal;
        let sampler = self
            .sampler
            .get()
            .cloned()
            .ok_or_else(|| rmcp::ErrorData::internal_error("sampling is unavailable", None))?;
        let cancel = Cancellation::new();
        let worker = cancel.clone();
        let mut task = tokio::task::spawn_blocking(move || sampler.sample(&origin, call, &worker));
        let outcome = tokio::select! {
            outcome = &mut task => outcome,
            _ = context.ct.cancelled() => {
                cancel.cancel();
                task.await
            }
        };
        let sampled = match outcome {
            Ok(Ok(sampled)) => sampled,
            Ok(Err(SamplingRefusal::InvalidRequest(message))) => {
                return Err(rmcp::ErrorData::invalid_params(message, None));
            }
            Ok(Err(SamplingRefusal::Unauthorized(message))) => {
                return Err(rmcp::ErrorData::invalid_request(message, None));
            }
            Ok(Err(SamplingRefusal::Failed(message))) => {
                return Err(rmcp::ErrorData::internal_error(message, None));
            }
            Err(_) => return Err(rmcp::ErrorData::internal_error("sampling failed", None)),
        };
        if sampled.text.len() > MAX_RESULT_TEXT_BYTES {
            return Err(rmcp::ErrorData::internal_error(
                "sampled text exceeds 1 MiB",
                None,
            ));
        }
        Ok(
            CreateMessageResult::new(WireMessage::assistant_text(sampled.text), sampled.model)
                .with_stop_reason(if sampled.truncated {
                    CreateMessageResult::STOP_REASON_END_MAX_TOKEN
                } else {
                    CreateMessageResult::STOP_REASON_END_TURN
                }),
        )
    }
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
    /// The server answered the request with an error: it is known not to have succeeded.
    Rejected,
}

const CONNECT_DEADLINE: Duration = Duration::from_secs(20);
const CALL_DEADLINE: Duration = Duration::from_secs(15);
const CATALOG_MAX: usize = 500;
const CATALOG_MAX_BYTES: usize = 1024 * 1024;
const MCP_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

fn charge_catalog_page(
    remaining: &mut usize,
    page: &impl serde::Serialize,
) -> Result<(), ConnectFault> {
    let bytes = serde_json::to_vec(page)
        .map_err(|_| ConnectFault::Protocol)?
        .len();
    *remaining = remaining.checked_sub(bytes).ok_or(ConnectFault::TooLarge)?;
    Ok(())
}

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
    sampler: SharedSampler,
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
            sampler: Arc::default(),
        }
    }

    fn connect_wire(
        &self,
        context: &InvocationContext,
        endpoint: &McpEndpoint,
        timeout: Duration,
    ) -> Result<McpWireSession, McpRemoteFault> {
        let client = SamplingClient::new(endpoint.sampling, self.sampler.clone());
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
                    .block_on(async {
                        tokio::time::timeout(
                            timeout,
                            McpWireSession::http(
                                &endpoint.0,
                                bearer.as_ref().map(|s| s.expose()),
                                client,
                            ),
                        )
                        .await
                    })
                    .map_err(|_| McpRemoteFault::Deadline)?
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
                    .block_on(async {
                        tokio::time::timeout(
                            timeout,
                            McpWireSession::stdio(
                                self.supervisor.clone(),
                                context,
                                native,
                                &policy,
                                client,
                            ),
                        )
                        .await
                    })
                    .map_err(|_| McpRemoteFault::Deadline)?
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
        ConnectFault::Rejected => McpRemoteFault::Rejected,
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

impl NativeMcpClients {
    fn slot(&self, endpoint: &McpEndpoint) -> Result<(Vec<u8>, SessionSlot), McpRemoteFault> {
        let key =
            serde_json::to_vec(&(endpoint.scope.key(), &endpoint.transport, endpoint.sampling))
                .map_err(|_| McpRemoteFault::InvalidConfiguration)?;
        let slot = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(endpoint.server_id.as_str().into())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
            .clone();
        Ok((key, slot))
    }

    /// Attach a server the host already connected over in-process pipes. The session then serves
    /// that endpoint exactly as a connection opened by `connect` would, sampling included.
    pub async fn attach_pipes(
        &self,
        endpoint: &McpEndpoint,
        reader: tokio::io::DuplexStream,
        writer: tokio::io::DuplexStream,
    ) -> Result<(), McpRemoteFault> {
        let (key, slot) = self.slot(endpoint)?;
        let client = SamplingClient::new(endpoint.sampling, self.sampler.clone());
        let client = connect_pipes(reader, writer, client)
            .await
            .map_err(map_connect_fault)?;
        let wire = McpWireSession {
            client,
            process: None,
        };
        if let Some(old) = slot.lock().await.replace(ConnectedSession { key, wire }) {
            let _ = old.wire.close().await;
        }
        Ok(())
    }

    fn call(
        &self,
        server_id: &Id,
        origin: Option<McpCallOrigin>,
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
        // The slot lock is held for the whole call, so at most one attributed call per server
        // is in flight and a sampling request can belong only to it.
        self.runtime.block_on(async {
            let guard = slot.lock().await;
            let session = guard.as_ref().ok_or(McpRemoteFault::Unreachable)?;
            session
                .wire
                .call_tool(origin, name, arguments)
                .await
                .map_err(map_connect_fault)
        })
    }
}

impl McpRemote for NativeMcpClients {
    fn connect(
        &self,
        context: &InvocationContext,
        endpoint: &McpEndpoint,
    ) -> Result<(), McpRemoteFault> {
        self.connect_with_timeout(context, endpoint, CONNECT_DEADLINE)
    }

    fn connect_with_timeout(
        &self,
        context: &InvocationContext,
        endpoint: &McpEndpoint,
        timeout: Duration,
    ) -> Result<(), McpRemoteFault> {
        let deadline = Instant::now() + timeout.min(CONNECT_DEADLINE);
        let remaining = || {
            let left = deadline.saturating_duration_since(Instant::now());
            (!left.is_zero())
                .then_some(left)
                .ok_or(McpRemoteFault::Deadline)
        };
        let (key, slot) = self.slot(endpoint)?;
        // Keep the same slot across reconnects. Concurrent callers either reuse this endpoint
        // or wait for the changed endpoint to finish replacing its session.
        let timeout = remaining()?;
        let mut guard = self
            .runtime
            .block_on(async { tokio::time::timeout(timeout, slot.lock()).await })
            .map_err(|_| McpRemoteFault::Deadline)?;
        if guard
            .as_ref()
            .is_some_and(|connected| connected.key == key && !connected.wire.client.is_closed())
        {
            return Ok(());
        }
        if let Some(old) = guard.take()
            && !old.wire.client.is_closed()
        {
            let timeout = remaining()?;
            self.runtime
                .block_on(async { tokio::time::timeout(timeout, old.wire.close()).await })
                .map_err(|_| McpRemoteFault::Deadline)?
                .map_err(map_connect_fault)?;
        }
        let session = self.connect_wire(context, endpoint, remaining()?)?;
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
        self.call(server_id, None, name, arguments)
    }

    fn call_tool_with_origin(
        &self,
        origin: &McpCallOrigin,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpRemoteFault> {
        self.call(&origin.server_id, Some(origin.clone()), name, arguments)
    }

    fn install_sampler(&self, sampler: Arc<dyn McpSampler>) {
        let _ = self.sampler.set(sampler);
    }

    fn read_resource(
        &self,
        server_id: &Id,
        uri: &str,
    ) -> Result<serde_json::Value, McpRemoteFault> {
        self.read_resource_with_timeout(server_id, uri, CALL_DEADLINE)
    }

    fn read_resource_with_timeout(
        &self,
        server_id: &Id,
        uri: &str,
        timeout: Duration,
    ) -> Result<serde_json::Value, McpRemoteFault> {
        let slot = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(server_id.as_str())
            .cloned()
            .ok_or(McpRemoteFault::Unreachable)?;
        self.runtime.block_on(async {
            tokio::time::timeout(timeout.min(CALL_DEADLINE), async {
                let guard = slot.lock().await;
                let session = guard.as_ref().ok_or(McpRemoteFault::Unreachable)?;
                session
                    .wire
                    .read_resource(uri)
                    .await
                    .map_err(map_connect_fault)
            })
            .await
            .map_err(|_| McpRemoteFault::Deadline)?
        })
    }

    fn get_prompt(
        &self,
        server_id: &Id,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpRemoteFault> {
        self.get_prompt_with_timeout(server_id, name, arguments, CALL_DEADLINE)
    }

    fn get_prompt_with_timeout(
        &self,
        server_id: &Id,
        name: &str,
        arguments: serde_json::Value,
        timeout: Duration,
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
            tokio::time::timeout(timeout.min(CALL_DEADLINE), async {
                let guard = slot.lock().await;
                let session = guard.as_ref().ok_or(McpRemoteFault::Unreachable)?;
                session
                    .wire
                    .get_prompt(name, arguments)
                    .await
                    .map_err(map_connect_fault)
            })
            .await
            .map_err(|_| McpRemoteFault::Deadline)?
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

    fn disconnect_if(
        &self,
        server_id: &Id,
        still_off: &mut dyn FnMut() -> bool,
    ) -> Result<(), McpRemoteFault> {
        let slot = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(server_id.as_str())
            .cloned();
        let Some(slot) = slot else {
            still_off();
            return Ok(());
        };
        // `connect` takes this lock before replacing a session, so the check and the close
        // see the same session.
        let mut guard = self.runtime.block_on(slot.lock());
        if !still_off() {
            return Ok(());
        }
        match guard.take() {
            Some(session) => self
                .runtime
                .block_on(session.wire.close())
                .map_err(map_connect_fault),
            None => Ok(()),
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

    /// Return the exact bounded protocol result. Sampling is accepted only for an attributed
    /// call to a selected server; elicitation is not advertised. Sampling inside an
    /// input-required result is not supported and stays a protocol fault.
    pub async fn call_tool(
        &self,
        origin: Option<McpCallOrigin>,
        name: &str,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, ConnectFault> {
        let params = CallToolRequestParams::new(name.to_owned()).with_arguments(arguments);
        let handler = self.client.service();
        let _in_flight = handler.begin(origin);
        let started = Instant::now();
        let call = self.client.call_tool_once(params);
        tokio::pin!(call);
        let response = loop {
            let until = handler.call_deadline(started);
            match tokio::time::timeout_at(until.into(), &mut call).await {
                Ok(response) => break response,
                Err(_) if handler.call_deadline(started) > until => continue,
                Err(_) => return Err(ConnectFault::Deadline),
            }
        }
        .map_err(|error| match error {
            // A JSON-RPC error reply is the server's answer, not a lost one.
            rmcp::ServiceError::McpError(_) => ConnectFault::Rejected,
            _ => ConnectFault::Protocol,
        })?;
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
            let mut remaining_bytes = CATALOG_MAX_BYTES;
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
                    charge_catalog_page(&mut remaining_bytes, &page)?;
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
                    charge_catalog_page(&mut remaining_bytes, &page)?;
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
                    charge_catalog_page(&mut remaining_bytes, &page)?;
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
    pub async fn http(
        endpoint: &str,
        bearer: Option<&str>,
        client: SamplingClient,
    ) -> Result<Self, ConnectFault> {
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
        let client = tokio::time::timeout(CONNECT_DEADLINE, serve_client(client, transport))
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
        client: SamplingClient,
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
        let client = connect_pipes(reader, writer, client).await?;
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
    client: SamplingClient,
) -> Result<RunningService<RoleClient, SamplingClient>, ConnectFault> {
    tokio::time::timeout(
        CONNECT_DEADLINE,
        serve_client(
            client,
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
    fn discovery_pages_share_one_byte_budget_across_capability_families() {
        let mut remaining = CATALOG_MAX_BYTES;
        charge_catalog_page(
            &mut remaining,
            &serde_json::json!({"tools": [{"description": "x".repeat(600_000)}], "nextCursor": "a"}),
        )
        .unwrap();
        assert_eq!(
            charge_catalog_page(
                &mut remaining,
                &serde_json::json!({"resources": [{"description": "y".repeat(500_000)}]}),
            ),
            Err(ConnectFault::TooLarge)
        );
    }

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
        let client = connect_pipes(
            client_reader,
            client_writer,
            SamplingClient::new(false, Arc::default()),
        )
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
                None,
                "read_note",
                serde_json::Map::from_iter([("id".into(), serde_json::json!("note-1"))]),
            )
            .await
            .expect("native tool call");
        assert_eq!(result["isError"], false);
        let _ = session.close().await;
        server.abort();
    }

    /// Lists one resource and one prompt with a required argument and answers both.
    struct ContentServer;
    impl rmcp::ServerHandler for ContentServer {
        fn get_info(&self) -> rmcp::model::ServerConfig {
            rmcp::model::ServerConfig::new(
                rmcp::model::ServerCapabilities::builder()
                    .enable_resources()
                    .enable_prompts()
                    .build(),
            )
        }

        async fn list_resources(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> Result<rmcp::model::ListResourcesResult, rmcp::ErrorData> {
            let mut result = rmcp::model::ListResourcesResult::default();
            result
                .resources
                .push(Resource::new("note://welcome", "Welcome note"));
            Ok(result)
        }

        async fn read_resource(
            &self,
            request: ReadResourceRequestParams,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> Result<ReadResourceResponse, rmcp::ErrorData> {
            Ok(ReadResourceResponse::Complete(
                rmcp::model::ReadResourceResult::new(vec![rmcp::model::ResourceContents::text(
                    "Hello from the note",
                    request.uri,
                )]),
            ))
        }

        async fn list_prompts(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> Result<rmcp::model::ListPromptsResult, rmcp::ErrorData> {
            let mut topic = rmcp::model::PromptArgument::new("topic");
            topic.required = Some(true);
            let mut result = rmcp::model::ListPromptsResult::default();
            result.prompts.push(Prompt::new(
                "summarize",
                Some("Summarize a topic"),
                Some(vec![topic]),
            ));
            Ok(result)
        }

        async fn get_prompt(
            &self,
            request: GetPromptRequestParams,
            _context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> Result<GetPromptResponse, rmcp::ErrorData> {
            let topic = request
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get("topic"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("nothing")
                .to_owned();
            Ok(GetPromptResponse::Complete(
                rmcp::model::GetPromptResult::new(vec![rmcp::model::PromptMessage::new_text(
                    rmcp::model::Role::User,
                    format!("Summarize {topic}"),
                )]),
            ))
        }
    }

    #[tokio::test]
    async fn the_native_client_discovers_and_reads_resources_and_prompts() {
        let (client_reader, server_writer) = tokio::io::duplex(64 * 1024);
        let (server_reader, client_writer) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            rmcp::serve_server(ContentServer, (server_reader, server_writer))
                .await
                .expect("local server handshake")
        });
        let client = connect_pipes(
            client_reader,
            client_writer,
            SamplingClient::new(false, Arc::default()),
        )
        .await
        .expect("native client handshake");
        let session = McpWireSession {
            client,
            process: None,
        };
        let catalog = session.discover().await.expect("discovery");
        assert!(catalog.tools.is_empty());
        assert_eq!(catalog.resources.len(), 1);
        assert_eq!(catalog.resources[0].uri, "note://welcome");
        assert_eq!(catalog.prompts.len(), 1);
        assert_eq!(catalog.prompts[0].name, "summarize");
        let argument = &catalog.prompts[0].arguments.as_ref().unwrap()[0];
        assert_eq!(
            (argument.name.as_str(), argument.required),
            ("topic", Some(true))
        );

        let resource = session
            .read_resource("note://welcome")
            .await
            .expect("resource read");
        assert_eq!(resource["contents"][0]["uri"], "note://welcome");
        assert_eq!(resource["contents"][0]["text"], "Hello from the note");
        let prompt = session
            .get_prompt(
                "summarize",
                serde_json::Map::from_iter([("topic".into(), serde_json::json!("the release"))]),
            )
            .await
            .expect("prompt read");
        assert_eq!(prompt["messages"][0]["role"], "user");
        assert_eq!(
            prompt["messages"][0]["content"]["text"],
            "Summarize the release"
        );
        let _ = session.close().await;
        server.abort();
    }

    #[derive(Default)]
    struct FakeSampler(Mutex<Vec<(McpCallOrigin, u8, usize)>>);
    impl McpSampler for FakeSampler {
        fn sample(
            &self,
            origin: &McpCallOrigin,
            call: SamplingCall,
            _cancel: &Cancellation,
        ) -> Result<devworks_core::ports::mcp::SampledText, SamplingRefusal> {
            self.0
                .lock()
                .unwrap()
                .push((origin.clone(), call.ordinal, call.messages.len()));
            let last = &call.messages.last().unwrap().text;
            if last == "deny" {
                return Err(SamplingRefusal::Unauthorized("mcp_sampling_authority"));
            }
            Ok(devworks_core::ports::mcp::SampledText {
                model: "fixture-model".into(),
                text: format!("sampled {last}"),
                truncated: last == "long",
            })
        }
    }

    #[allow(deprecated)]
    fn sample(text: &str) -> rmcp::model::CreateMessageRequestParams {
        rmcp::model::CreateMessageRequestParams::new(
            vec![rmcp::model::SamplingMessage::user_text(text)],
            64,
        )
    }

    /// A server whose tool call sends each listed sampling request and reports every outcome.
    #[allow(deprecated)]
    struct SamplingServer(Vec<rmcp::model::CreateMessageRequestParams>);
    impl rmcp::ServerHandler for SamplingServer {
        fn get_info(&self) -> rmcp::model::ServerConfig {
            rmcp::model::ServerConfig::new(
                rmcp::model::ServerCapabilities::builder()
                    .enable_tools()
                    .build(),
            )
        }

        async fn call_tool(
            &self,
            _request: CallToolRequestParams,
            context: rmcp::service::RequestContext<rmcp::RoleServer>,
        ) -> Result<CallToolResponse, rmcp::ErrorData> {
            let mut outcomes = Vec::new();
            for params in self.0.clone() {
                outcomes.push(request_sample(&context.peer, params).await);
            }
            Ok(CallToolResponse::Complete(
                rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                    serde_json::Value::Array(outcomes).to_string(),
                )]),
            ))
        }
    }

    #[allow(deprecated)]
    async fn request_sample(
        peer: &rmcp::Peer<rmcp::RoleServer>,
        params: rmcp::model::CreateMessageRequestParams,
    ) -> serde_json::Value {
        use rmcp::model::{ClientResult, CreateMessageRequest, ServerRequest};
        match peer
            .send_request(ServerRequest::CreateMessageRequest(
                CreateMessageRequest::new(params),
            ))
            .await
        {
            Ok(ClientResult::CreateMessageResult(result)) => serde_json::json!({
                "text": result.message.content.first().and_then(|c| c.as_text()).map(|t| &t.text),
                "stop": result.stop_reason, "model": result.model,
            }),
            Ok(_) => serde_json::json!({"error": "unexpected result"}),
            Err(error) => serde_json::json!({"error": error.to_string()}),
        }
    }

    async fn sampling_pair(
        server: SamplingServer,
        client: SamplingClient,
    ) -> (
        McpWireSession,
        rmcp::service::RunningService<rmcp::RoleServer, SamplingServer>,
    ) {
        let (client_reader, server_writer) = tokio::io::duplex(64 * 1024);
        let (server_reader, client_writer) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(async move {
            rmcp::serve_server(server, (server_reader, server_writer))
                .await
                .expect("local server handshake")
        });
        let client = connect_pipes(client_reader, client_writer, client)
            .await
            .expect("native client handshake");
        (
            McpWireSession {
                client,
                process: None,
            },
            server.await.unwrap(),
        )
    }

    fn origin() -> McpCallOrigin {
        McpCallOrigin {
            server_id: Id::new("sampling-server").unwrap(),
            operation_id: devworks_core::protocol::OperationId(Id::new("mcp-call").unwrap()),
            attempt_id: Id::new("journal-attempt").unwrap(),
        }
    }

    fn outcomes(result: serde_json::Value) -> Vec<serde_json::Value> {
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn sampling_is_advertised_only_when_the_server_selection_allows_it() {
        for selected in [false, true] {
            let (session, server) = sampling_pair(
                SamplingServer(vec![sample("hello")]),
                SamplingClient::new(selected, Arc::default()),
            )
            .await;
            let advertised = server
                .peer()
                .peer_info()
                .unwrap()
                .capabilities
                .sampling
                .clone();
            assert_eq!(advertised.is_some(), selected);
            if let Some(capability) = advertised {
                assert!(capability.tools.is_none() && capability.context.is_none());
            }
            if !selected {
                // An unselected server cannot sample even inside an attributed call.
                let result = session
                    .call_tool(Some(origin()), "sample", serde_json::Map::new())
                    .await
                    .unwrap();
                assert!(outcomes(result)[0]["error"].is_string());
            }
            let _ = session.close().await;
            let _ = server.cancel().await;
        }
    }

    #[tokio::test]
    #[allow(deprecated)]
    async fn attributed_call_routes_valid_samples_and_refuses_the_rest_without_sampling() {
        use rmcp::model::{ContextInclusion, SamplingMessage as Wire, SamplingMessageContentBlock};
        let mut include_context = sample("context");
        include_context.include_context = Some(ContextInclusion::ThisServer);
        let mut tools = sample("tools");
        tools.tools = Some(vec![Tool::default()]);
        let mut image = sample("image");
        image.messages = vec![Wire::new(
            rmcp::model::Role::User,
            SamplingMessageContentBlock::Image(rmcp::model::ImageContent::new("AA==", "image/png")),
        )];
        let mut zero = sample("zero");
        zero.max_tokens = 0;
        let mut large = sample("large");
        large.max_tokens = 4097;
        let mut hot = sample("hot");
        hot.temperature = Some(1.5);
        let mut stops = sample("stops");
        stops.stop_sequences = Some(vec!["x".into(); 5]);
        let mut assistant_last = sample("first");
        assistant_last.messages.push(Wire::assistant_text("last"));
        let requests = vec![
            sample("one"),
            include_context,
            tools,
            image,
            zero,
            large,
            hot,
            stops,
            assistant_last,
            sample("deny"),
            sample("three"),
            sample("long"),
            sample("five"),
        ];
        let sampler = Arc::new(FakeSampler::default());
        let installed: SharedSampler = Arc::default();
        let _ = installed.set(sampler.clone());
        let (session, server) = sampling_pair(
            SamplingServer(requests),
            SamplingClient::new(true, installed),
        )
        .await;
        let result = session
            .call_tool(Some(origin()), "sample", serde_json::Map::new())
            .await
            .expect("tool call");
        let outcomes = outcomes(result);
        assert_eq!(outcomes[0]["text"], "sampled one");
        assert_eq!(outcomes[0]["stop"], "endTurn");
        assert_eq!(outcomes[0]["model"], "fixture-model");
        for refused in &outcomes[1..10] {
            assert!(refused["error"].is_string(), "{refused}");
        }
        assert_eq!(outcomes[10]["text"], "sampled three");
        assert_eq!(outcomes[11]["stop"], "maxTokens");
        assert!(outcomes[12]["error"].as_str().unwrap().contains("limit"));
        let calls = sampler.0.lock().unwrap().clone();
        assert_eq!(
            calls
                .iter()
                .map(|(_, ordinal, _)| *ordinal)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert!(
            calls
                .iter()
                .all(|(call, _, messages)| *call == origin() && *messages == 1)
        );

        // Outside an attributed call the server's request never reaches the sampler.
        let unsolicited = request_sample(server.peer(), sample("idle")).await;
        assert!(unsolicited["error"].as_str().unwrap().contains("tool call"));
        let unattributed = session
            .call_tool(None, "sample", serde_json::Map::new())
            .await
            .unwrap();
        assert!(
            outcomes_of(unattributed)
                .iter()
                .all(|o| o["error"].is_string())
        );
        assert_eq!(sampler.0.lock().unwrap().len(), 4);
        let _ = session.close().await;
        let _ = server.cancel().await;
    }

    fn outcomes_of(result: serde_json::Value) -> Vec<serde_json::Value> {
        outcomes(result)
    }

    #[test]
    fn each_accepted_sample_extends_its_call_deadline() {
        let client = SamplingClient::new(true, Arc::default());
        let started = Instant::now();
        let _call = client.begin(Some(origin()));
        assert_eq!(client.call_deadline(started) - started, CALL_DEADLINE);
        for samples in 1..=4 {
            client.admit(true).unwrap();
            assert_eq!(
                client.call_deadline(started) - started,
                CALL_DEADLINE + SAMPLE_BUDGET * samples
            );
        }
        assert!(client.admit(true).is_err());
        drop(_call);
        assert!(client.admit(false).is_err());
    }

    #[tokio::test]
    async fn credential_cannot_be_sent_to_cleartext_remote_or_url_userinfo() {
        assert_eq!(
            McpWireSession::http(
                "http://example.test/mcp",
                Some("secret"),
                SamplingClient::new(false, Arc::default())
            )
            .await
            .err(),
            Some(ConnectFault::InvalidEndpoint)
        );
        assert_eq!(
            McpWireSession::http(
                "https://user:pass@example.test/mcp",
                None,
                SamplingClient::new(false, Arc::default())
            )
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
            McpWireSession::http(&endpoint, None, SamplingClient::new(false, Arc::default()))
                .await
                .err(),
            Some(ConnectFault::Handshake)
        );
    }
}
