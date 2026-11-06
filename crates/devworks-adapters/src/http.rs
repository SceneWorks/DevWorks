//! The native [`HttpTransport`] (DW-18): blocking HTTPS through ureq with rustls (ring) and the
//! bundled Mozilla roots. Redirects are never followed (a 3xx is returned as the response, so an
//! authorization header never reaches another origin); every request has its own global timeout
//! and a response-size limit that fails instead of truncating. Error values name the failure
//! class only, never a URL, header or body.
use devworks_core::ports::http::{
    HttpError, HttpExchange, HttpRequest, HttpResponse, HttpTransport, MAX_RESPONSE_METADATA_BYTES,
    Method,
};
use std::time::Duration;

pub struct NativeHttp {
    agent: ureq::Agent,
}

impl NativeHttp {
    /// The production transport: the user's proxy environment (`HTTPS_PROXY`, `NO_PROXY`, ...)
    /// applies, as it does for other native tools.
    pub fn new() -> Self {
        Self::build(true)
    }
    /// A transport that never uses a proxy (tests against an in-process server).
    pub fn direct() -> Self {
        Self::build(false)
    }
    fn build(env_proxy: bool) -> Self {
        let mut config = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .user_agent("DevWorks");
        if !env_proxy {
            config = config.proxy(None);
        }
        Self {
            agent: config.build().into(),
        }
    }
}
impl Default for NativeHttp {
    fn default() -> Self {
        Self::new()
    }
}

fn classify(e: &ureq::Error) -> HttpError {
    match e {
        ureq::Error::Timeout(_) => HttpError::Timeout,
        ureq::Error::BodyExceedsLimit(_) => HttpError::TooLarge,
        ureq::Error::HostNotFound => HttpError::Unreachable("the host was not found".into()),
        ureq::Error::ConnectionFailed => HttpError::Unreachable("the connection failed".into()),
        ureq::Error::Tls(_) | ureq::Error::Rustls(_) => {
            HttpError::Unreachable("the TLS handshake failed".into())
        }
        ureq::Error::Io(io) if io.kind() == std::io::ErrorKind::TimedOut => HttpError::Timeout,
        ureq::Error::Io(_) => {
            HttpError::Unreachable("an I/O error interrupted the exchange".into())
        }
        ureq::Error::BadUri(_) | ureq::Error::Http(_) => {
            HttpError::InvalidRequest("the URL or a header is invalid".into())
        }
        _ => HttpError::Unreachable("the HTTP exchange failed".into()),
    }
}

impl HttpTransport for NativeHttp {
    fn send(&self, r: &HttpRequest) -> Result<HttpResponse, HttpError> {
        self.exchange(r, false).map(|exchange| exchange.response)
    }
    fn send_with_headers(&self, r: &HttpRequest) -> Result<HttpExchange, HttpError> {
        self.exchange(r, true)
    }
}
impl NativeHttp {
    fn exchange(&self, r: &HttpRequest, metadata: bool) -> Result<HttpExchange, HttpError> {
        let timeout = Some(Duration::from_millis(r.timeout_ms.max(1)));
        let result = match r.method {
            Method::Get => {
                let mut b = self.agent.get(&r.url);
                for (name, value) in &r.headers {
                    b = b.header(name.as_str(), value.as_str());
                }
                b.config().timeout_global(timeout).build().call()
            }
            Method::Post => {
                let mut b = self.agent.post(&r.url);
                for (name, value) in &r.headers {
                    b = b.header(name.as_str(), value.as_str());
                }
                let body: &[u8] = r.body.as_deref().unwrap_or_default();
                b.config().timeout_global(timeout).build().send(body)
            }
            Method::Patch => {
                let mut b = self.agent.patch(&r.url);
                for (name, value) in &r.headers {
                    b = b.header(name.as_str(), value.as_str());
                }
                let body: &[u8] = r.body.as_deref().unwrap_or_default();
                b.config().timeout_global(timeout).build().send(body)
            }
        };
        let mut response = result.map_err(|e| classify(&e))?;
        let status = response.status().as_u16();
        let mut headers = Vec::new();
        let mut retained = 0usize;
        let names: &[&str] = if metadata {
            &[
                "etag",
                "link",
                "date",
                "x-poll-interval",
                "retry-after",
                "x-ratelimit-limit",
                "x-ratelimit-remaining",
                "x-ratelimit-reset",
                "x-ratelimit-resource",
                "x-oauth-scopes",
                "x-accepted-oauth-scopes",
                "x-github-sso",
                "x-github-request-id",
            ]
        } else {
            &[]
        };
        for &name in names {
            for value in response.headers().get_all(name) {
                let value = value.to_str().map_err(|_| {
                    HttpError::Unreachable("the response metadata was invalid".into())
                })?;
                retained = retained
                    .checked_add(name.len() + value.len())
                    .filter(|bytes| *bytes <= MAX_RESPONSE_METADATA_BYTES)
                    .ok_or(HttpError::TooLarge)?;
                headers.push((name.to_owned(), value.to_owned()));
            }
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(r.max_response_bytes as u64)
            .read_to_vec()
            .map_err(|e| classify(&e))?;
        Ok(HttpExchange {
            response: HttpResponse { status, body },
            headers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// One request as an in-process server saw it.
    struct Seen {
        line: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    /// Serves `responses` in order on loopback, reporting each request it read.
    fn serve(responses: Vec<(Duration, String)>) -> (String, mpsc::Receiver<Seen>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for (delay, response) in responses {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut headers = Vec::new();
                let mut length = 0usize;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    let h = h.trim_end().to_owned();
                    if h.is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap();
                    if k.eq_ignore_ascii_case("content-length") {
                        length = v.trim().parse().unwrap();
                    }
                    headers.push((k.to_ascii_lowercase(), v.trim().to_owned()));
                }
                let mut body = vec![0u8; length];
                reader.read_exact(&mut body).unwrap();
                let _ = tx.send(Seen {
                    line: line.trim_end().to_owned(),
                    headers,
                    body,
                });
                std::thread::sleep(delay);
                let mut stream = stream;
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (base, rx)
    }
    fn ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn conditional_exchange_preserves_control_headers_without_logging_values() {
        let (base, seen) = serve(vec![(Duration::ZERO,
            "HTTP/1.1 304 Not Modified\r\nETag: W/\"opaque-private\"\r\nLink: </page2>; rel=\"next\"\r\nLink: </page9>; rel=\"last\"\r\nX-Poll-Interval: 60\r\nRetry-After: 120\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: 12345\r\nX-OAuth-Scopes: repo,read:org\r\nSet-Cookie: private-cookie\r\nAuthorization: private-response-token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into())]);
        let exchange = NativeHttp::direct()
            .send_with_headers(
                &HttpRequest::get(format!("{base}/issues"))
                    .header("If-None-Match", "W/\"opaque-private\""),
            )
            .unwrap();
        assert_eq!(exchange.response.status, 304);
        assert!(exchange.response.body.is_empty());
        assert_eq!(
            exchange.header_values("ETAG").collect::<Vec<_>>(),
            ["W/\"opaque-private\""]
        );
        assert_eq!(
            exchange.header_values("link").collect::<Vec<_>>(),
            ["</page2>; rel=\"next\"", "</page9>; rel=\"last\""]
        );
        for (name, value) in [
            ("x-poll-interval", "60"),
            ("retry-after", "120"),
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset", "12345"),
            ("x-oauth-scopes", "repo,read:org"),
        ] {
            assert_eq!(exchange.header_values(name).collect::<Vec<_>>(), [value]);
        }
        assert!(exchange.header_values("set-cookie").next().is_none());
        assert!(exchange.header_values("authorization").next().is_none());
        let debug = format!("{exchange:?}");
        for value in [
            "opaque-private",
            "private-cookie",
            "private-response-token",
            "repo,read:org",
            "/page2",
        ] {
            assert!(!debug.contains(value));
        }
        assert!(
            seen.recv()
                .unwrap()
                .headers
                .iter()
                .any(|(name, value)| name == "if-none-match" && value == "W/\"opaque-private\"")
        );
    }

    #[test]
    fn header_exchange_keeps_body_bound_and_rejects_oversized_metadata() {
        let metadata = (0..18)
            .map(|_| format!("ETag: {}\r\n", "x".repeat(2048)))
            .collect::<String>();
        let (base, _) = serve(vec![
            (Duration::ZERO, ok(&"x".repeat(32))),
            (
                Duration::ZERO,
                format!(
                    "HTTP/1.1 200 OK\r\n{metadata}Content-Length: 0\r\nConnection: close\r\n\r\n"
                ),
            ),
            (
                Duration::ZERO,
                format!(
                    "HTTP/1.1 200 OK\r\n{metadata}Content-Length: 0\r\nConnection: close\r\n\r\n"
                ),
            ),
        ]);
        let http = NativeHttp::direct();
        let mut request = HttpRequest::get(format!("{base}/bounded"));
        request.max_response_bytes = 4;
        assert!(matches!(
            http.send_with_headers(&request),
            Err(HttpError::TooLarge)
        ));
        assert!(matches!(
            http.send_with_headers(&request),
            Err(HttpError::TooLarge)
        ));
        // Existing callers do not opt into retaining or validating response metadata.
        assert_eq!(http.send(&request).unwrap().status, 200);
    }

    #[test]
    fn sends_headers_and_bodies_and_reads_the_response() {
        let (base, seen) = serve(vec![
            (Duration::ZERO, ok("{\"data\":[]}")),
            (Duration::ZERO, ok("{\"access_token\":\"t\"}")),
        ]);
        let http = NativeHttp::direct();
        let r = http
            .send(
                &HttpRequest::get(format!("{base}/v1/models")).secret_header(
                    "Authorization",
                    devworks_core::ports::secrets::Secret::new("Bearer k"),
                ),
            )
            .unwrap();
        assert_eq!(
            (r.status, r.body.as_slice()),
            (200, b"{\"data\":[]}".as_slice())
        );
        let s = seen.recv().unwrap();
        assert_eq!(s.line, "GET /v1/models HTTP/1.1");
        assert!(
            s.headers
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer k")
        );
        http.send(&HttpRequest::post(
            format!("{base}/oauth/token"),
            "application/x-www-form-urlencoded",
            b"grant_type=refresh_token".to_vec(),
        ))
        .unwrap();
        let s = seen.recv().unwrap();
        assert_eq!(s.line, "POST /oauth/token HTTP/1.1");
        assert_eq!(s.body, b"grant_type=refresh_token");
    }

    #[test]
    fn patch_preserves_method_body_and_secret_header() {
        let (base, seen) = serve(vec![(Duration::ZERO, ok("{}"))]);
        let mut request = HttpRequest::post(
            format!("{base}/repos/o/r/pulls/1"),
            "application/json",
            br#"{"base":"feature"}"#.to_vec(),
        )
        .secret_header(
            "Authorization",
            devworks_core::ports::secrets::Secret::new("Bearer private"),
        );
        request.method = Method::Patch;
        NativeHttp::direct().send(&request).unwrap();
        let sent = seen.recv().unwrap();
        assert_eq!(sent.line, "PATCH /repos/o/r/pulls/1 HTTP/1.1");
        assert_eq!(sent.body, br#"{"base":"feature"}"#);
        assert!(
            sent.headers
                .iter()
                .any(|(key, value)| key == "authorization" && value == "Bearer private")
        );
        assert!(!format!("{request:?}").contains("Bearer private"));
    }

    #[test]
    fn redirects_are_returned_not_followed() {
        let (base, seen) = serve(vec![(
            Duration::ZERO,
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        )]);
        let r = NativeHttp::direct()
            .send(&HttpRequest::get(format!("{base}/models")))
            .unwrap();
        assert_eq!(r.status, 302);
        seen.recv().unwrap();
        // Only the one request was made.
        assert!(seen.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn oversized_bodies_and_slow_answers_fail_instead_of_truncating() {
        let big = "x".repeat(4096);
        let (base, _seen) = serve(vec![
            (Duration::ZERO, ok(&big)),
            (Duration::from_millis(1500), ok("{}")),
        ]);
        let http = NativeHttp::direct();
        let mut r = HttpRequest::get(format!("{base}/big"));
        r.max_response_bytes = 1024;
        assert_eq!(http.send(&r).unwrap_err(), HttpError::TooLarge);
        let mut r = HttpRequest::get(format!("{base}/slow"));
        r.timeout_ms = 200;
        assert_eq!(http.send(&r).unwrap_err(), HttpError::Timeout);
    }

    #[test]
    fn an_unreachable_endpoint_is_an_availability_error() {
        // Bind and drop a listener so the port is closed.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let err = NativeHttp::direct()
            .send(&HttpRequest::get(format!("http://127.0.0.1:{port}/models")))
            .unwrap_err();
        assert!(matches!(err, HttpError::Unreachable(_)), "{err:?}");
    }
}
