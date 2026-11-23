//! Desktop-owned incoming MCP. The SDK handles the negotiated protocol; the application
//! gateway owns admission, execution and all durable effects.

mod context;
mod http;
#[cfg(any(unix, windows))]
pub(crate) mod ipc;

use crate::platform::{HostIds, SystemClock};
use devworks_core::context::InvocationContext;
use devworks_core::ports::Clock;
use devworks_core::ports::storage::{Cancellation, ReadOptions, WriteOptions};
use devworks_core::protocol::records::{ItemState, McpInboundStatus, McpServerBinding};
use devworks_core::protocol::results::{DomainDecision, DomainQueryReply};
use devworks_core::protocol::support::BoundedText;
use devworks_core::protocol::{CommandReply, Id, Revision};
use devworks_core::registry::Mode;
use devworks_core::services::mcp_inbound::{
    AuthenticatedClient, Credentials, PublishedTool, publication_allowed, published_tools,
};
use devworks_engine::runtime::Runtime;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ErrorCode, ErrorData, Implementation,
    InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const REQUEST_LIMIT: Duration = Duration::from_secs(30);
const MAX_RESULT_BYTES: usize = 512 * 1024;
const TOOLS_PER_PAGE: usize = 12;
const VERSIONS: [ProtocolVersion; 2] =
    [ProtocolVersion::V_2026_07_28, ProtocolVersion::V_2025_11_25];

#[derive(Clone)]
pub(crate) struct IncomingMcp {
    runtime: Arc<Runtime>,
    pub(crate) credentials: Arc<Credentials>,
    ids: Arc<HostIds>,
    requests: Arc<Semaphore>,
    clients: Arc<Mutex<HashMap<Id, Weak<Semaphore>>>>,
    stop: CancellationToken,
    bindings: Arc<Mutex<NativeBindings>>,
}

#[derive(Default)]
struct NativeBindings {
    http: Option<Result<SocketAddr, &'static str>>,
    stdio: Option<Result<(), &'static str>>,
}

impl IncomingMcp {
    pub(crate) fn start(
        runtime: Arc<Runtime>,
        credentials: Arc<Credentials>,
        ids: Arc<HostIds>,
        data_root: &std::path::Path,
    ) -> Self {
        let this = Self {
            runtime,
            credentials,
            ids,
            requests: Arc::new(Semaphore::new(16)),
            clients: Arc::default(),
            stop: CancellationToken::new(),
            bindings: Arc::default(),
        };
        tokio::spawn(http::supervise(this.clone()));
        #[cfg(any(unix, windows))]
        tokio::spawn(ipc::supervise(this.clone(), data_root.to_owned()));
        #[cfg(not(any(unix, windows)))]
        let _ = data_root;
        this
    }

    pub(crate) fn close(&self) {
        self.credentials.close();
        self.stop.cancel();
    }

    pub(crate) fn report(&self, status: &mut McpInboundStatus) {
        if !status.settings.enabled || self.stop.is_cancelled() {
            return;
        }
        let Ok(bindings) = self.bindings.lock() else {
            return;
        };
        let mut available = 0;
        let mut faults = Vec::new();
        for binding in &status.settings.bindings.0 {
            match binding {
                McpServerBinding::Stdio => match bindings.stdio {
                    Some(Ok(())) => available += 1,
                    Some(Err(reason)) => faults.push(reason),
                    None => faults.push("Stdio pairing is starting."),
                },
                McpServerBinding::LoopbackHttp => match bindings.http {
                    Some(Ok(address))
                        if status
                            .settings
                            .http_port
                            .as_ref()
                            .is_some_and(|port| port.get() == u32::from(address.port())) =>
                    {
                        status.endpoint = Some(
                            serde_json::from_value(serde_json::json!(format!(
                                "http://{address}/mcp"
                            )))
                            .expect("loopback endpoint is a valid URL"),
                        );
                        available += 1;
                    }
                    Some(Err(reason)) => faults.push(reason),
                    _ => faults.push("Loopback HTTP is starting."),
                },
            }
        }
        status.state = match (available, faults.is_empty()) {
            (0, _) => ItemState::Unavailable,
            (_, true) => ItemState::Enabled,
            (_, false) => ItemState::Partial,
        };
        if available > 0 {
            status.protocol_versions.0 = VERSIONS
                .iter()
                .map(|version| BoundedText(version.as_str().to_owned()))
                .collect();
        }
        status.diagnostic = (!faults.is_empty()).then(|| BoundedText(faults.join(" ")));
    }

    fn tools(
        &self,
        client: &AuthenticatedClient,
        options: &ReadOptions,
    ) -> Result<Vec<&'static PublishedTool>, ErrorData> {
        let mut tools = self
            .credentials
            .granted_tools(client, options)
            .map_err(|_| denied())?;
        tools.retain(|tool| self.available(tool));
        Ok(tools)
    }

    fn run_tool(
        &self,
        proof: &RequestProof,
        name: &str,
        input: Value,
        options: &ReadOptions,
    ) -> Result<(Value, bool), ErrorData> {
        let tool = self
            .tools(&proof.client, options)?
            .into_iter()
            .find(|tool| tool.name == name)
            .ok_or_else(|| ErrorData::invalid_params("Unknown tool", None))?;
        let id = Id::new(self.ids.token("mcp")).expect("host invocation identifier");
        let selected = tool
            .request(&id, input)
            .map_err(|_| ErrorData::invalid_params("Invalid DevWorks tool arguments", None))?;
        let ctx = proof.client.invocation(
            id,
            selected.project,
            Revision::new(1).expect("initial revision"),
        );
        let operation = tool.operation.name;
        let (value, error, protected) = match tool.binding.mode {
            Mode::Query => {
                let reply = self
                    .runtime
                    .query_with(&ctx, operation, &selected.frame, options)
                    .map_err(|_| unavailable())?;
                let success = matches!(reply, DomainQueryReply::Result { .. });
                (serde_json::to_value(reply), !success, true)
            }
            Mode::Command => {
                let reply = self
                    .runtime
                    .command_with(
                        &ctx,
                        operation,
                        &selected.frame,
                        WriteOptions {
                            cancellation: options.cancellation.clone(),
                            deadline_ms: options.deadline_ms,
                            ..WriteOptions::command()
                        },
                    )
                    .map_err(|_| unavailable())?;
                let success = matches!(reply, CommandReply::Recorded { ref receipt, .. }
                        if !matches!(receipt.decision, DomainDecision::Refused { .. }));
                let protected = matches!(
                    reply,
                    CommandReply::Recorded { .. } | CommandReply::ReceiptPurged { .. }
                );
                (serde_json::to_value(reply), !success, protected)
            }
        };
        let value = value.map_err(|_| unavailable())?;
        if value.to_string().len() > MAX_RESULT_BYTES {
            return Err(ErrorData::new(
                ErrorCode(-32010),
                "DevWorks result exceeds the transport limit. Use a smaller page or recover the command by its application request key.",
                None,
            ));
        }
        if protected {
            let _ = proof.delivery.set(Delivery::Operation(ctx));
        }
        Ok((value, error))
    }

    fn available(&self, tool: &PublishedTool) -> bool {
        self.runtime.has_mcp_tool(tool) && tool.has_target_authority()
    }

    async fn execute<T: Send + 'static>(
        &self,
        request: CancellationToken,
        client: Arc<AuthenticatedClient>,
        work: impl FnOnce(Self, ReadOptions) -> Result<T, ErrorData> + Send + 'static,
    ) -> Result<T, ErrorData> {
        let capacity = Arc::clone(&self.requests)
            .try_acquire_owned()
            .map_err(|_| busy())?;
        let quota = {
            let mut clients = self.clients.lock().map_err(|_| unavailable())?;
            clients.retain(|_, quota| quota.strong_count() > 0);
            let quota = clients.entry(client.id().clone()).or_default();
            match quota.upgrade() {
                Some(quota) => quota,
                None => {
                    let semaphore = Arc::new(Semaphore::new(4));
                    *quota = Arc::downgrade(&semaphore);
                    semaphore
                }
            }
        };
        let client_capacity = quota.try_acquire_owned().map_err(|_| busy())?;
        let cancellation = Cancellation::new();
        let options = ReadOptions {
            cancellation: Some(cancellation.clone()),
            deadline_ms: Some(
                SystemClock
                    .now_ms()
                    .get()
                    .saturating_add(REQUEST_LIMIT.as_millis() as i64),
            ),
        };
        let _guard = CancelOnDrop(cancellation);
        let this = self.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _capacity = capacity;
            let _client_capacity = client_capacity;
            work(this, options)
        });
        tokio::select! {
            result = task => result.map_err(|_| unavailable())?,
            _ = request.cancelled() => Err(ErrorData::new(ErrorCode(-32800), "Request cancelled", None)),
            _ = self.stop.cancelled() => Err(unavailable()),
            _ = tokio::time::sleep(REQUEST_LIMIT) => Err(ErrorData::new(ErrorCode(-32010), "Request deadline exceeded", None)),
        }
    }
}

struct CancelOnDrop(Cancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Clone)]
struct RequestProof {
    client: Arc<AuthenticatedClient>,
    delivery: Arc<OnceLock<Delivery>>,
}

enum Delivery {
    Discovery,
    Operation(InvocationContext),
}

impl RequestProof {
    fn new(client: AuthenticatedClient) -> Self {
        Self {
            client: Arc::new(client),
            delivery: Arc::default(),
        }
    }

    fn check(&self, native: &IncomingMcp) -> bool {
        let options = ReadOptions {
            deadline_ms: Some(SystemClock.now_ms().get().saturating_add(1000)),
            cancellation: None,
        };
        match self.delivery.get() {
            Some(Delivery::Discovery) => native.tools(&self.client, &options).is_ok(),
            Some(Delivery::Operation(ctx)) => {
                publication_allowed(ctx, &**native.runtime.store(), &options)
            }
            None => true,
        }
    }
}

struct Server(IncomingMcp);

impl Server {
    fn proof(context: &RequestContext<RoleServer>) -> Result<RequestProof, ErrorData> {
        if !context
            .protocol_version()
            .as_ref()
            .is_some_and(|version| VERSIONS.contains(version))
        {
            return Err(ErrorData::invalid_request(
                "Unsupported MCP protocol version",
                None,
            ));
        }
        context
            .extensions
            .get::<RequestProof>()
            .or_else(|| {
                context
                    .extensions
                    .get::<hyper::http::request::Parts>()?
                    .extensions
                    .get::<RequestProof>()
            })
            .cloned()
            .ok_or_else(denied)
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_server_info(Implementation::new("DevWorks", crate::VERSION))
        .with_protocol_version(VERSIONS[0].clone())
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&VERSIONS)
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        if request.protocol_version != VERSIONS[1] {
            return Err(ErrorData::unsupported_protocol_version(
                request.protocol_version,
                &VERSIONS,
            ));
        }
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    async fn list_tools(
        &self,
        params: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let proof = Self::proof(&context)?;
        let cursor = params.and_then(|params| params.cursor);
        self.0
            .execute(context.ct, proof.client.clone(), move |native, options| {
                native
                    .credentials
                    .granted_tools(&proof.client, &options)
                    .map_err(|_| denied())?;
                let tools: Vec<_> = published_tools()
                    .iter()
                    .filter(|tool| native.available(tool))
                    .collect();
                let offset = match cursor {
                    Some(cursor) => {
                        tools
                            .iter()
                            .position(|tool| tool.name == cursor)
                            .ok_or_else(|| ErrorData::invalid_params("Invalid tool cursor", None))?
                            + 1
                    }
                    None => 0,
                };
                let items = &tools[offset..tools.len().min(offset + TOOLS_PER_PAGE)];
                let mut page = ListToolsResult::with_all_items(
                    items.iter().map(|tool| definition(tool)).collect(),
                );
                if offset + items.len() < tools.len() {
                    page.next_cursor = items.last().map(|tool| tool.name.clone());
                }
                let _ = proof.delivery.set(Delivery::Discovery);
                Ok(page)
            })
            .await
    }

    async fn list_resource_templates(
        &self,
        _: Option<PaginatedRequestParams>,
        request: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListResourceTemplatesResult, ErrorData> {
        context::list_resource_templates(self, request).await
    }

    async fn read_resource(
        &self,
        params: rmcp::model::ReadResourceRequestParams,
        request: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, ErrorData> {
        context::read_resource(self, params, request).await
    }

    async fn list_prompts(
        &self,
        _: Option<PaginatedRequestParams>,
        request: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListPromptsResult, ErrorData> {
        context::list_prompts(self, request).await
    }

    async fn get_prompt(
        &self,
        params: rmcp::model::GetPromptRequestParams,
        request: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::GetPromptResponse, ErrorData> {
        context::get_prompt(self, params, request).await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let proof = Self::proof(&context)?;
        self.0
            .execute(context.ct, proof.client.clone(), move |native, options| {
                let (value, error) = native.run_tool(
                    &proof,
                    &request.name,
                    Value::Object(request.arguments.unwrap_or_default()),
                    &options,
                )?;
                Ok(if error {
                    CallToolResult::structured_error(value)
                } else {
                    CallToolResult::structured(value)
                }
                .into())
            })
            .await
    }
}

fn definition(tool: &'static PublishedTool) -> Tool {
    static DEFINITIONS: OnceLock<Mutex<HashMap<&'static str, Tool>>> = OnceLock::new();
    let mut definitions = DEFINITIONS
        .get_or_init(Mutex::default)
        .lock()
        .expect("schema cache");
    definitions
        .entry(&tool.name)
        .or_insert_with(|| {
            let schema = tool
                .input_schema()
                .as_object()
                .expect("tool argument object")
                .clone();
            let mut definition = Tool::new(
                tool.name.clone(),
                format!("{} ({})", tool.operation.name, tool.binding.mode.as_str()),
                schema,
            );
            definition.annotations =
                Some(ToolAnnotations::new().read_only(tool.binding.mode == Mode::Query));
            definition.output_schema = Some(Arc::new(
                tool.output_schema()
                    .as_object()
                    .expect("tool result object")
                    .clone(),
            ));
            definition
        })
        .clone()
}

fn busy() -> ErrorData {
    ErrorData::new(ErrorCode(-32010), "DevWorks is busy", None)
}

fn denied() -> ErrorData {
    ErrorData::new(ErrorCode(-32001), "DevWorks client access denied", None)
}

fn unavailable() -> ErrorData {
    ErrorData::internal_error("DevWorks incoming MCP is unavailable", None)
}
