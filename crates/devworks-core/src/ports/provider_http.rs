//! Incremental provider HTTP. Implementations never follow redirects, log credentials/bodies,
//! truncate a response, or return from `close` while local I/O is still running.
use super::http::HttpRequest;
use super::storage::Cancellation;
use std::{fmt, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendPhase {
    NotSent,
    AfterSend,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportFailure {
    Cancelled,
    Deadline,
    IdleTimeout,
    TooLarge,
    InvalidRequest,
    Unreachable,
    Protocol,
}
/// Classified only: never put provider bodies, URLs or secret headers in an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportError {
    pub phase: SendPhase,
    pub kind: TransportFailure,
}
#[derive(Clone, Debug)]
pub struct StreamLimits {
    pub deadline_ms: u64,
    pub idle_timeout_ms: u64,
    pub max_response_bytes: u64,
    pub max_header_bytes: usize,
    pub max_headers: usize,
}
impl StreamLimits {
    pub fn validate(&self) -> bool {
        self.deadline_ms > 0
            && self.idle_timeout_ms > 0
            && self.idle_timeout_ms <= self.deadline_ms
            && self.max_response_bytes > 0
            && self.max_header_bytes > 0
            && self.max_headers > 0
    }
}
/// Header values can include provider metadata: Debug deliberately prints names only.
#[derive(Clone)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}
impl ResponseHead {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}
impl fmt::Debug for ResponseHead {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseHead")
            .field("status", &self.status)
            .field(
                "header_names",
                &self.headers.iter().map(|(n, _)| n).collect::<Vec<_>>(),
            )
            .finish()
    }
}
/// Independently usable interruption handle. Idempotent; shuts down a blocked native read.
pub trait StreamCloser: Send + Sync {
    fn close(&self) -> Result<(), TransportError>;
}
pub trait HttpStream: Send {
    fn closer(&self) -> Arc<dyn StreamCloser>;
    fn head(&self) -> &ResponseHead;
    /// Zero means EOF, never temporary absence. Must observe cancellation while waiting.
    fn read(
        &mut self,
        buffer: &mut [u8],
        cancellation: &Cancellation,
    ) -> Result<usize, TransportError>;
    fn close(&mut self) -> Result<(), TransportError>;
}
pub trait StreamingHttp: Send + Sync {
    fn open(
        &self,
        request: &HttpRequest,
        limits: &StreamLimits,
        cancellation: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError>;
}
