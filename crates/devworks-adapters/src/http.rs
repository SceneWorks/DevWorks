//! The native [`HttpTransport`] (DW-18): blocking HTTPS through ureq with rustls (ring) and the
//! bundled Mozilla roots. Redirects are never followed (a 3xx is returned as the response, so an
//! authorization header never reaches another origin); every request has its own global timeout
//! and a response-size limit that fails instead of truncating. Error values name the failure
//! class only, never a URL, header or body.
use devworks_core::ports::http::{HttpError, HttpRequest, HttpResponse, HttpTransport, Method};
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
        };
        let mut response = result.map_err(|e| classify(&e))?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .with_config()
            .limit(r.max_response_bytes as u64)
            .read_to_vec()
            .map_err(|e| classify(&e))?;
        Ok(HttpResponse { status, body })
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
