//! The network boundary of the shared web tools (`web.search`, `web.fetch`; DW-51).
//!
//! Core owns every egress decision (which URL, which address, which redirect, which bytes are
//! kept); adapters own only the wire. The split is what makes the decisions testable without a
//! network and what keeps DNS rebinding out:
//!
//! * a [`Resolver`] answers a domain once per hop; core classifies **every** address it returned
//!   and refuses the hop if any is disallowed;
//! * the [`HttpTransport`] connects to exactly the [`SocketAddr`] core checked. It never resolves,
//!   never follows a redirect, never stores or sends a cookie and never adds an authorization or
//!   proxy header: it sends the fixed request core describes and returns the response head and a
//!   bounded body stream;
//! * a [`SearchProvider`] is an external search API reached with its own configured credential.
//!
//! Every fault says whether request bytes may have reached the server ([`NetFault::dispatched`]),
//! so a failure after dispatch is never reported as one before it.
use crate::ports::secrets::SecretValue;
use crate::ports::storage::Cancellation;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

/// Why a network step produced no complete response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetFault {
    /// Nothing was sent: name resolution, connection or TLS handshake failed, or the deadline
    /// passed before the request was written.
    NotSent { reason: String, timed_out: bool },
    /// Request bytes were (or may have been) written and no complete response arrived: a timeout,
    /// reset or malformed reply after dispatch. What the server did is unknown.
    AfterSend { reason: String, timed_out: bool },
    /// Stopped by cancellation or shutdown; `sent` says whether request bytes may have reached the
    /// server.
    Cancelled { sent: bool },
}
impl NetFault {
    /// Whether request bytes may have reached the server.
    pub fn dispatched(&self) -> bool {
        match self {
            Self::NotSent { .. } => false,
            Self::AfterSend { .. } => true,
            Self::Cancelled { sent } => *sent,
        }
    }
    pub fn timed_out(&self) -> bool {
        matches!(
            self,
            Self::NotSent {
                timed_out: true,
                ..
            } | Self::AfterSend {
                timed_out: true,
                ..
            }
        )
    }
    pub fn reason(&self) -> &str {
        match self {
            Self::NotSent { reason, .. } | Self::AfterSend { reason, .. } => reason,
            Self::Cancelled { .. } => "cancelled",
        }
    }
}

/// Resolves a domain to its addresses.
pub trait Resolver: Send + Sync {
    /// Resolves `host` (an ASCII domain, never an IP literal) once, within `deadline`, stopping
    /// when any of `cancel` is set. An empty answer is a fault, not success.
    fn resolve(
        &self,
        host: &str,
        port: u16,
        deadline: Instant,
        cancel: &[Cancellation],
    ) -> Result<Vec<IpAddr>, NetFault>;
}

/// The URL scheme of one hop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

/// The TLS server name a hop verifies the certificate against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerName {
    Dns(String),
    Ip(IpAddr),
}

/// One `GET` the transport sends to an already-checked address.
#[derive(Clone)]
pub struct HopRequest<'a> {
    pub scheme: Scheme,
    /// The `Host` header value (`example.com`, `example.com:8443`, `[2001:db8::1]:8080`).
    pub host_header: &'a str,
    /// What the TLS certificate is verified against (HTTPS only).
    pub server_name: &'a ServerName,
    /// The origin-form request target (`/path?query`), already percent-encoded, no fragment.
    pub target: &'a str,
    /// The address core checked; the only one the transport may connect to.
    pub addr: SocketAddr,
    /// Connect, handshake, write and every body read end by this instant.
    pub deadline: Instant,
    /// Stops the exchange between bounded waits (shutdown or caller cancellation).
    pub cancel: Vec<Cancellation>,
    pub user_agent: &'a str,
    /// The `Accept` header value.
    pub accept: &'a str,
    /// Further header fields: only a search provider's own credential header. A fetch sends
    /// none.
    pub headers: &'a [(&'a str, &'a str)],
}

/// A response head and its body stream.
pub struct HopResponse {
    pub status: u16,
    /// Header fields in received order (names as sent; at most the transport's bound).
    pub headers: Vec<(String, String)>,
    /// The decoded (de-chunked) body. A read past the hop deadline fails with
    /// `ErrorKind::TimedOut`, a cancelled one with `ErrorKind::Interrupted`; every error after
    /// this point is after dispatch.
    pub body: Box<dyn Read + Send>,
}
impl HopResponse {
    /// The first header named `name` (ASCII case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}
/// A request's `Debug` never shows a header value (a provider credential travels there) or the
/// request target (its query may carry secrets).
impl std::fmt::Debug for HopRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HopRequest")
            .field("scheme", &self.scheme)
            .field("host_header", &self.host_header)
            .field("addr", &self.addr)
            .field(
                "headers",
                &self.headers.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for HopResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HopResponse")
            .field("status", &self.status)
            .field("headers", &self.headers.len())
            .finish_non_exhaustive()
    }
}

/// Sends one hop. Implementations must not resolve names, follow redirects, keep cookies or add
/// credentials, and must bound the response head they accept.
pub trait HttpTransport: Send + Sync {
    fn get(&self, request: &HopRequest<'_>) -> Result<HopResponse, NetFault>;
}

/// One page request to a search provider.
pub struct SearchRequest<'a> {
    pub query: &'a str,
    /// Zero-based index of the first result.
    pub offset: u32,
    /// Results wanted (1..=[`SearchProvider::page_max`]).
    pub count: u32,
    /// The provider credential resolved from the native secret store, when it needs one.
    pub credential: Option<&'a SecretValue>,
    pub deadline: Instant,
    pub cancel: Vec<Cancellation>,
}

/// One result as the provider reported it (untrusted, unvalidated).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// One provider page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    /// The provider reports more results after this page.
    pub more: bool,
}

/// Why a search produced no page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchFault {
    Net(NetFault),
    /// The provider answered and refused the credential.
    CredentialRejected,
    /// The provider answered with a rate limit.
    RateLimited,
    /// The provider answered with another failure status.
    Rejected {
        status: u16,
    },
    /// The provider answered with something that is not its documented result format.
    Malformed(String),
}

/// An external search API.
pub trait SearchProvider: Send + Sync {
    /// The provider ID the `web.searchProvider` setting and `web.search` `providerId` name.
    fn id(&self) -> &'static str;
    /// Whether a configured credential is required.
    fn requires_credential(&self) -> bool;
    /// Most results one request returns.
    fn page_max(&self) -> u32;
    /// Highest offset the provider accepts; results beyond it are unreachable.
    fn offset_max(&self) -> u32;
    fn search(&self, request: &SearchRequest<'_>) -> Result<SearchPage, SearchFault>;
}
