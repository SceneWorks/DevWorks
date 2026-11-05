//! Outbound HTTP for provider authentication, model discovery and connection tests (SEC-03,
//! AD 39). Core never opens a socket: every request goes through this port, so tests use local
//! fakes or an in-process server and never the real network.
//!
//! The port is blocking and bounded: each request carries its own timeout and response-size
//! limit. Adapters never follow redirects (a 3xx is returned as the response), so an
//! authorization header can never be forwarded to another origin; callers treat a redirect as a
//! failure. Header values marked secret, and every request body, are never printed by `Debug`.
use super::secrets::Secret;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Patch,
}

/// A header value: plain, or a credential that must never be printed.
#[derive(Clone)]
pub enum HeaderValue {
    Plain(String),
    Secret(Secret),
}
impl HeaderValue {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Plain(s) => s,
            Self::Secret(s) => s.expose(),
        }
    }
}
impl fmt::Debug for HeaderValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain(s) => write!(f, "{s:?}"),
            Self::Secret(_) => f.write_str("<redacted>"),
        }
    }
}

#[derive(Clone)]
pub struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, HeaderValue)>,
    /// The body (JSON or form) may carry codes or refresh tokens; it is never printed.
    pub body: Option<Vec<u8>>,
    pub timeout_ms: u64,
    /// Larger responses fail with [`HttpError::TooLarge`] instead of being truncated.
    pub max_response_bytes: usize,
}
impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: Method::Get,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_response_bytes: DEFAULT_MAX_RESPONSE,
        }
    }
    pub fn post(url: impl Into<String>, content_type: &str, body: Vec<u8>) -> Self {
        let mut r = Self::get(url);
        r.method = Method::Post;
        r.body = Some(body);
        r.header("Content-Type", content_type)
    }
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers
            .push((name.to_owned(), HeaderValue::Plain(value.to_owned())));
        self
    }
    pub fn secret_header(mut self, name: &str, value: Secret) -> Self {
        self.headers
            .push((name.to_owned(), HeaderValue::Secret(value)));
        self
    }
    /// The plain value of a header (tests and fakes).
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}
impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &redact_query(&self.url))
            .field("headers", &self.headers)
            .field("body_bytes", &self.body.as_ref().map(Vec::len))
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

/// `url` without its query (a query may carry an authorization code).
pub fn redact_query(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

/// The default bound of one auth/discovery request.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// The default response bound (model lists of large catalogs fit comfortably).
pub const DEFAULT_MAX_RESPONSE: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}
impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A token response body holds credentials.
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}
impl HttpResponse {
    pub fn success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// A transport failure; no response was received. Never carries a URL query, header or body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpError {
    /// The URL or a header cannot be sent.
    InvalidRequest(String),
    /// DNS, connect, TLS or I/O failure: an availability event, never a credential revocation.
    Unreachable(String),
    Timeout,
    /// The response exceeded `max_response_bytes`.
    TooLarge,
}
impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(why) => write!(f, "the request is invalid ({why})"),
            Self::Unreachable(why) => write!(f, "the endpoint is unreachable ({why})"),
            Self::Timeout => f.write_str("the endpoint did not answer in time"),
            Self::TooLarge => f.write_str("the response was larger than allowed"),
        }
    }
}

/// Blocking outbound HTTP.
pub trait HttpTransport: Send + Sync {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, HttpError>;
}

/// No network at all: every request is unreachable (a composition without HTTP, and tests that
/// must prove nothing is sent).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoNetwork;
impl HttpTransport for NoNetwork {
    fn send(&self, _: &HttpRequest) -> Result<HttpResponse, HttpError> {
        Err(HttpError::Unreachable(
            "no network transport is configured".into(),
        ))
    }
}

/// Cryptographically secure randomness (PKCE verifiers and sign-in state). The production
/// adapter reads the OS generator.
pub trait Entropy: Send + Sync {
    fn fill(&self, buf: &mut [u8]);
}

/// Unpadded base64url (RFC 4648 section 5), for PKCE and JWT segments.
pub fn base64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// Decodes base64url with or without padding (`None` on any invalid character).
pub fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        } as u32)
    }
    let s = s.trim_end_matches('=').as_bytes();
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// `application/x-www-form-urlencoded` encoding of `pairs`.
pub fn form(pairs: &[(&str, &str)]) -> Vec<u8> {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent(k), percent(v)))
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes()
}

/// Percent-encodes everything but RFC 3986 unreserved characters.
pub fn percent(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_and_matches_rfc7636() {
        for len in 0..40 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(base64url_decode(&base64url(&bytes)).unwrap(), bytes);
        }
        // RFC 7636 appendix B: verifier bytes and the S256 challenge of its encoding.
        let verifier_bytes = [
            116u8, 24, 223, 180, 151, 153, 224, 37, 79, 250, 96, 125, 216, 173, 187, 186, 22, 212,
            37, 77, 105, 214, 191, 240, 91, 88, 5, 88, 83, 132, 141, 121,
        ];
        let verifier = base64url(&verifier_bytes);
        assert_eq!(verifier, "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        use sha2::Digest;
        let challenge = base64url(&sha2::Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn debug_never_prints_secrets_bodies_or_queries() {
        let r = HttpRequest::post(
            "https://auth.example.invalid/token?code=abc",
            "application/json",
            b"{\"refresh_token\":\"rt-secret\"}".to_vec(),
        )
        .secret_header("Authorization", Secret::new("Bearer at-secret"));
        let text = format!("{r:?}");
        assert!(!text.contains("rt-secret") && !text.contains("at-secret"));
        assert!(!text.contains("code=abc"));
        let resp = HttpResponse {
            status: 200,
            body: b"{\"access_token\":\"x\"}".to_vec(),
        };
        assert!(!format!("{resp:?}").contains("access_token"));
    }

    #[test]
    fn form_encoding_escapes_reserved_characters() {
        assert_eq!(
            String::from_utf8(form(&[("a b", "c&d=e/f")])).unwrap(),
            "a%20b=c%26d%3De%2Ff"
        );
    }
}
