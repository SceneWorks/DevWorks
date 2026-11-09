//! Outgoing MCP protocol boundary. The host supplies a native implementation; core owns
//! configuration, authority, receipts and publication. Remote descriptions are untrusted data.
use crate::context::InvocationContext;
use crate::ports::storage::ConfigScope;
use crate::protocol::Id;
use crate::protocol::records::McpTransport;
use serde_json::Value;

#[derive(Clone)]
pub struct McpEndpoint {
    pub server_id: Id,
    pub scope: ConfigScope,
    pub transport: McpTransport,
}

pub struct RemoteTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    /// A remote hint for initial user selection, never proof of a verified read.
    pub read_only_hint: bool,
}

pub struct RemoteResource {
    pub uri: String,
    pub name: String,
    pub description: String,
}

pub struct RemotePrompt {
    pub name: String,
    pub description: String,
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
}

pub trait McpRemote: Send + Sync {
    /// Reconnects or starts one server. The caller journals its intent before this step.
    fn connect(
        &self,
        context: &InvocationContext,
        endpoint: &McpEndpoint,
    ) -> Result<(), McpRemoteFault>;
    /// Reads a bounded complete catalog from a connected server.
    fn discover(&self, server_id: &Id) -> Result<RemoteCatalog, McpRemoteFault>;
    /// One selected tool call. The core service records the effect before invoking this port.
    fn call_tool(
        &self,
        server_id: &Id,
        name: &str,
        arguments: Value,
    ) -> Result<Value, McpRemoteFault>;
    fn read_resource(&self, server_id: &Id, uri: &str) -> Result<Value, McpRemoteFault>;
    fn get_prompt(
        &self,
        server_id: &Id,
        name: &str,
        arguments: Value,
    ) -> Result<Value, McpRemoteFault>;
    /// Drops a connection and stops its app-owned process tree when present.
    fn disconnect(&self, server_id: &Id) -> Result<(), McpRemoteFault>;
}
