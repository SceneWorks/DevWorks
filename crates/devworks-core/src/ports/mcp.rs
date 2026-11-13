//! Outgoing MCP protocol boundary. The host supplies a native implementation; core owns
//! configuration, authority, receipts and publication. Remote descriptions are untrusted data.
use crate::context::InvocationContext;
use crate::ports::storage::{Cancellation, ConfigScope};
use crate::protocol::records::McpTransport;
use crate::protocol::{Id, OperationId};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct McpEndpoint {
    pub server_id: Id,
    pub scope: ConfigScope,
    pub transport: McpTransport,
    /// A human selected this server's sampling capability. Only then does the client
    /// advertise MCP sampling during the handshake.
    pub sampling: bool,
}

/// The journaled `mcp.tool` dispatch attempt a server request arrives under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpCallOrigin {
    pub server_id: Id,
    pub operation_id: OperationId,
    pub attempt_id: Id,
}

pub struct SamplingMessage {
    pub assistant: bool,
    pub text: String,
}

/// One validated text-only sampling request. The router, not the server, chooses the model.
pub struct SamplingCall {
    /// 1-based position of this request within its tool call.
    pub ordinal: u8,
    pub messages: Vec<SamplingMessage>,
    pub system_prompt: Option<String>,
    pub max_tokens: u32,
    pub temperature: Option<f64>,
    pub stop_sequences: Vec<String>,
}

pub struct SampledText {
    pub model: String,
    pub text: String,
    /// The output token limit ended generation.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SamplingRefusal {
    /// The calling tool operation no longer holds the authority to sample.
    Unauthorized(&'static str),
    InvalidRequest(&'static str),
    /// Inference was refused or did not complete; no partial text is returned.
    Failed(&'static str),
}

pub trait McpSampler: Send + Sync {
    fn sample(
        &self,
        origin: &McpCallOrigin,
        call: SamplingCall,
        cancel: &Cancellation,
    ) -> Result<SampledText, SamplingRefusal>;
}

pub struct RemoteTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    /// A remote hint for initial user selection, never proof of a verified read.
    pub read_only_hint: bool,
    /// The tool's human-readable title, when the server gives one.
    pub title: Option<String>,
    /// The server's untrusted destructiveHint, when it gives one.
    pub destructive_hint: Option<bool>,
}

pub struct RemoteResource {
    pub uri: String,
    pub name: String,
    pub description: String,
}

pub struct RemotePrompt {
    pub name: String,
    pub description: String,
    pub arguments: Vec<RemotePromptArgument>,
}

pub struct RemotePromptArgument {
    pub name: String,
    pub required: bool,
}

pub struct RemoteCatalog {
    pub tools: Vec<RemoteTool>,
    pub resources: Vec<RemoteResource>,
    pub prompts: Vec<RemotePrompt>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpRemoteFault {
    InvalidConfiguration,
    MissingCredential,
    UnavailableRuntime,
    LaunchRefused,
    Unreachable,
    Protocol,
    Deadline,
    TooLarge,
    InputRequired,
    /// The server answered with an error, so the request did not succeed.
    Rejected,
}

pub trait McpRemote: Send + Sync {
    /// Reconnects or starts one server. The caller journals its intent before this step.
    fn connect(
        &self,
        context: &InvocationContext,
        endpoint: &McpEndpoint,
    ) -> Result<(), McpRemoteFault>;
    fn connect_with_timeout(
        &self,
        context: &InvocationContext,
        endpoint: &McpEndpoint,
        _timeout: Duration,
    ) -> Result<(), McpRemoteFault> {
        self.connect(context, endpoint)
    }
    /// Reads a bounded complete catalog from a connected server.
    fn discover(&self, server_id: &Id) -> Result<RemoteCatalog, McpRemoteFault>;
    /// One selected tool call. The core service records the effect before invoking this port.
    fn call_tool(
        &self,
        server_id: &Id,
        name: &str,
        arguments: Value,
    ) -> Result<Value, McpRemoteFault>;
    /// A tool call during which the server may request sampling for `origin`.
    fn call_tool_with_origin(
        &self,
        origin: &McpCallOrigin,
        name: &str,
        arguments: Value,
    ) -> Result<Value, McpRemoteFault> {
        self.call_tool(&origin.server_id, name, arguments)
    }
    /// The host's router-backed sampler. A remote without sampling support ignores it.
    fn install_sampler(&self, _sampler: Arc<dyn McpSampler>) {}
    fn read_resource(&self, server_id: &Id, uri: &str) -> Result<Value, McpRemoteFault>;
    fn read_resource_with_timeout(
        &self,
        server_id: &Id,
        uri: &str,
        _timeout: Duration,
    ) -> Result<Value, McpRemoteFault> {
        self.read_resource(server_id, uri)
    }
    fn get_prompt(
        &self,
        server_id: &Id,
        name: &str,
        arguments: Value,
    ) -> Result<Value, McpRemoteFault>;
    fn get_prompt_with_timeout(
        &self,
        server_id: &Id,
        name: &str,
        arguments: Value,
        _timeout: Duration,
    ) -> Result<Value, McpRemoteFault> {
        self.get_prompt(server_id, name, arguments)
    }
    /// Drops a connection and stops its app-owned process tree when present.
    fn disconnect(&self, server_id: &Id) -> Result<(), McpRemoteFault>;
    /// Drops the connection only when `still_off` answers true. A remote that serializes
    /// reconnects evaluates it while holding the server's session lock, so a reconnect
    /// cannot slip between the check and the close.
    fn disconnect_if(
        &self,
        server_id: &Id,
        still_off: &mut dyn FnMut() -> bool,
    ) -> Result<(), McpRemoteFault> {
        if still_off() {
            self.disconnect(server_id)
        } else {
            Ok(())
        }
    }
}
