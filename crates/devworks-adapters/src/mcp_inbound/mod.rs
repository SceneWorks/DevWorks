//! Native incoming MCP admission. Protocol parsing and publishing use this boundary.

use devworks_core::ports::secrets::Secret;
use devworks_core::services::mcp_inbound::{
    AuthenticatedClient, Binding, CredentialError, Credentials,
};
use hyper::header::{self, HeaderName};
use hyper::http::request::Parts;
use std::net::SocketAddr;

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

pub const MAX_REQUEST_BYTES: u64 = 1_048_576;
const MAX_HEADER_BYTES: usize = 16_384;

/// Authenticate an HTTP request after the listener supplies the actual local and peer
/// addresses. Both come from the connection, never from proxy or Host headers.
pub fn authenticate_http(
    credentials: &Credentials,
    local: SocketAddr,
    peer: SocketAddr,
    request: &Parts,
) -> Result<AuthenticatedClient, CredentialError> {
    if !local.ip().is_loopback()
        || !peer.ip().is_loopback()
        || request.uri.path() != "/mcp"
        || request.uri.query().is_some()
        || request.headers.contains_key(header::ORIGIN)
        || request
            .headers
            .iter()
            .map(|(name, value)| name.as_str().len() + value.len())
            .sum::<usize>()
            > MAX_HEADER_BYTES
    {
        return Err(CredentialError::Denied);
    }
    let host = one_header(request, header::HOST)?
        .and_then(|host| host.parse::<SocketAddr>().ok())
        .ok_or(CredentialError::Denied)?;
    if host != local
        || request
            .uri
            .authority()
            .is_some_and(|target| target.as_str() != local.to_string())
    {
        return Err(CredentialError::Denied);
    }
    if let Some(length) = one_header(request, header::CONTENT_LENGTH)?
        && length
            .parse::<u64>()
            .ok()
            .is_none_or(|length| length > MAX_REQUEST_BYTES)
    {
        return Err(CredentialError::Denied);
    }
    let value = one_header(request, header::AUTHORIZATION)?.ok_or(CredentialError::Denied)?;
    let (scheme, bearer) = value.split_once(' ').ok_or(CredentialError::Denied)?;
    if !scheme.eq_ignore_ascii_case("bearer") || !bearer.is_ascii() {
        return Err(CredentialError::Denied);
    }
    credentials.authenticate(
        Binding::LoopbackHttp { port: local.port() },
        &Secret::new(bearer),
    )
}

fn one_header(request: &Parts, name: HeaderName) -> Result<Option<&str>, CredentialError> {
    let values = request.headers.get_all(name);
    let mut values = values.iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(CredentialError::Denied);
    }
    first
        .map(|value| value.to_str().map_err(|_| CredentialError::Denied))
        .transpose()
}
