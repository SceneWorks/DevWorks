//! Native provider preparation. Internal contracts, separate from public inference/package DTOs.
//! Preparation resolves immutable material but never opens sockets or grants dispatch authority.
pub mod capabilities;
pub mod context;
pub mod decode;
pub mod effort;
pub mod encode;
pub mod instructions;
pub mod limits;
pub mod materialize;
pub mod sse;
use crate::protocol::provider::{EndpointProfile, NativeBinding, QualifiedId, SchemaRef};
use crate::protocol::{AttemptId, Id};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderError {
    InvalidInput(&'static str),
    Unsupported(&'static str),
    MaterialUnavailable,
    ForeignNative,
    TooLarge,
    Protocol(&'static str),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResponseMode {
    Streaming,
    Aggregate,
}
#[derive(Clone, Debug)]
pub struct RequestIdentity {
    pub request_id: Id,
    pub attempt_id: AttemptId,
    pub binding: NativeBinding,
    /// Exact upstream ID; a catalog ID or alias is never sent in its place.
    pub upstream_model: String,
}
#[derive(Clone, Debug)]
pub struct PreparedTool {
    pub wire_name: String,
    pub capability_id: QualifiedId,
    pub input_schema: SchemaRef,
    pub schema: Value,
    pub description: Option<String>,
}
/// Host-observed HTTP metadata, kept distinct from every provider-controlled native event.
#[derive(Clone, Debug)]
pub struct ProviderResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub access: ProviderAccessBasis,
}
#[derive(Clone, Debug)]
pub struct ProviderAccessBasis {
    pub connection_id: Id,
    pub profile: EndpointProfile,
    pub endpoint_digest: String,
    pub credential_revision: i64,
    pub account_revision: i64,
    pub oauth: bool,
}

/// Credential-free prepared bytes. The host obtains current Access and injects secrets at send.
#[derive(Clone)]
pub struct PreparedRequest {
    pub identity: RequestIdentity,
    pub profile: EndpointProfile,
    pub path: &'static str,
    pub body: Vec<u8>,
    /// Nonsecret pinned profile headers only. Authentication is injected at dispatch.
    pub headers: Vec<(String, String)>,
    pub response_mode: ResponseMode,
    pub tools: Vec<PreparedTool>,
    pub embedding_dimensions: Option<u32>,
    /// Stable per-session affinity for provider-specific send-time headers.
    pub session_key: Option<String>,
    /// Every exact source read, for the host's final current-visibility/metadata fence.
    pub artifact_refs: Vec<crate::protocol::provider::ArtifactRef>,
}
impl std::fmt::Debug for PreparedRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedRequest")
            .field("profile", &self.profile)
            .field("path", &self.path)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

#[cfg(test)]
mod tests;
