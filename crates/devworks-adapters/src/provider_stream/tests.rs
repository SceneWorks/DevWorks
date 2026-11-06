use super::*;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};

fn limits() -> StreamLimits {
    StreamLimits {
        deadline_ms: 2_000,
        idle_timeout_ms: 1_000,
        max_request_bytes: 64 * 1024,
        max_response_bytes: 64 * 1024,
        max_header_bytes: 8192,
        max_headers: 32,
    }
}
trait Wire: std::io::Read + std::io::Write + Send {}
impl<T: std::io::Read + std::io::Write + Send> Wire for T {}
struct Server {
    address: std::net::SocketAddr,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(f: impl FnOnce(TcpStream) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(4);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("local fixture accept failed: {}", e.kind()),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            f(stream);
        });
        Self {
            address,
            thread: Some(thread),
        }
    }
    fn url(&self) -> String {
        format!("http://{}/rpc?private=query-secret", self.address)
    }
    fn finish(mut self) {
        self.thread.take().unwrap().join().unwrap();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn request_bytes(stream: &mut impl std::io::Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut byte = [0];
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 65536);
        assert_eq!(stream.read(&mut byte).unwrap(), 1);
        bytes.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&bytes).to_lowercase();
    let length = head
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length:")
                .map(|s| s.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    bytes.extend(body);
    bytes
}
fn all(stream: &mut dyn HttpStream) -> Result<Vec<u8>, TransportError> {
    let mut body = Vec::new();
    let mut part = [0; 3];
    loop {
        let n = stream.read(&mut part, &Cancellation::new())?;
        if n == 0 {
            return Ok(body);
        }
        body.extend_from_slice(&part[..n]);
    }
}
fn expect_open_error(
    client: &NativeStreamingHttp,
    request: &HttpRequest,
    limits: &StreamLimits,
) -> TransportError {
    match client.open(request, limits, &Cancellation::new()) {
        Ok(mut stream) => {
            stream.close().unwrap();
            panic!("expected refusal");
        }
        Err(error) => error,
    }
}

#[test]
fn http_incremental_chunked_head_status_and_secret_placement() {
    let (sent, captured) = mpsc::channel();
    let server = Server::new(move |mut socket| {
        sent.send(request_bytes(&mut socket)).unwrap();
        socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\nRetry-After: 2\r\nX-Request-Id: private-id\r\nSet-Cookie: never-store=secret\r\n\r\n").unwrap();
        for bytes in [
            b"2\r\nda\r\n".as_slice(),
            b"6\r\nta: x\n\r\n",
            b"1\r\n\n\r\n",
            b"0\r\n\r\n",
        ] {
            socket.write_all(bytes).unwrap();
            socket.flush().unwrap();
        }
    });
    let request = HttpRequest::post(server.url(), "application/json", b"body-secret".to_vec())
        .secret_header(
            "Authorization",
            devworks_core::ports::secrets::Secret::new("bearer-secret"),
        );
    let client = NativeStreamingHttp::direct().unwrap();
    let mut stream = client
        .open(&request, &limits(), &Cancellation::new())
        .unwrap();
    assert_eq!(stream.head().status, 429);
    assert_eq!(stream.head().header("retry-after"), Some("2"));
    assert!(stream.head().header("set-cookie").is_none());
    assert!(!format!("{:?} {client:?} {request:?}", stream.head()).contains("private-id"));
    assert!(!format!("{client:?} {request:?}").contains("bearer-secret"));
    assert_eq!(all(&mut *stream).unwrap(), b"data: x\n\n");
    stream.close().unwrap();
    let bytes = String::from_utf8(captured.recv_timeout(Duration::from_secs(2)).unwrap()).unwrap();
    assert!(bytes.contains("authorization: bearer-secret\r\n"));
    assert!(bytes.ends_with("body-secret"));
    assert!(bytes.contains("connection: close\r\n"));
    server.finish();
}
#[test]
fn http_returns_redirect_without_forwarding_credentials() {
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let destination = target.local_addr().unwrap();
    let server = Server::new(move |mut socket| {
        request_bytes(&mut socket);
        write!(socket, "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{destination}/stolen\r\nContent-Length: 0\r\n\r\n").unwrap();
    });
    let client = NativeStreamingHttp::direct().unwrap();
    let mut stream = client
        .open(
            &HttpRequest::get(server.url()).header("Authorization", "credential"),
            &limits(),
            &Cancellation::new(),
        )
        .unwrap();
    assert_eq!(stream.head().status, 307);
    assert_eq!(all(&mut *stream).unwrap(), b"");
    assert_eq!(
        target.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    server.finish();
}
fn stalled(mut socket: impl Wire) {
    request_bytes(&mut socket);
    disconnected_after_head(socket);
}
fn disconnected_after_head(mut socket: impl Wire) {
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\n\r\n",
        )
        .unwrap();
    socket.flush().unwrap();
    let mut byte = [0];
    match socket.read(&mut byte) {
        Ok(0) => {}
        Err(e)
            if !matches!(
                e.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) => {}
        result => panic!("client did not physically disconnect: {result:?}"),
    }
}
#[test]
fn http_independent_close_interrupts_blocked_read_and_disconnects_server() {
    let server = Server::new(stalled);
    let client = NativeStreamingHttp::direct().unwrap();
    let mut stream = client
        .open(
            &HttpRequest::get(server.url()),
            &limits(),
            &Cancellation::new(),
        )
        .unwrap();
    let closer = stream.closer();
    let (result, received) = mpsc::channel();
    let reader = thread::spawn(move || {
        result
            .send(stream.read(&mut [0; 8], &Cancellation::new()))
            .unwrap();
    });
    assert_eq!(
        received.recv_timeout(Duration::from_millis(20)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    let start = Instant::now();
    closer.close().unwrap();
    closer.close().unwrap();
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        Err(after(TransportFailure::Cancelled))
    );
    reader.join().unwrap();
    server.finish();
    assert!(start.elapsed() < Duration::from_secs(1));
}
#[test]
fn http_cancellation_and_idle_failure_disconnect_stalled_bodies() {
    for cancel in [true, false] {
        let server = Server::new(stalled);
        let client = NativeStreamingHttp::direct().unwrap();
        let mut bounds = limits();
        bounds.idle_timeout_ms = 80;
        let mut stream = client
            .open(
                &HttpRequest::get(server.url()),
                &bounds,
                &Cancellation::new(),
            )
            .unwrap();
        let cancellation = Cancellation::new();
        let trigger = cancellation.clone();
        let read = thread::spawn(move || stream.read(&mut [0; 8], &cancellation));
        if cancel {
            trigger.cancel();
        }
        assert_eq!(
            read.join().unwrap(),
            Err(after(if cancel {
                TransportFailure::Cancelled
            } else {
                TransportFailure::IdleTimeout
            }))
        );
        server.finish();
    }
}
#[test]
fn http_deadline_closes_even_when_consumer_is_not_reading() {
    let server = Server::new(stalled);
    let client = NativeStreamingHttp::direct().unwrap();
    let mut bounds = limits();
    bounds.deadline_ms = 90;
    bounds.idle_timeout_ms = 90;
    let mut stream = client
        .open(
            &HttpRequest::get(server.url()),
            &bounds,
            &Cancellation::new(),
        )
        .unwrap();
    server.finish(); // physical EOF follows the deadline without asking for another read
    assert_eq!(
        stream.read(&mut [0; 8], &Cancellation::new()),
        Err(after(TransportFailure::Deadline))
    );
}
#[test]
fn http_request_validation_and_precancel_are_proven_not_sent() {
    let client = NativeStreamingHttp::direct().unwrap();
    let mut bounds = limits();
    bounds.max_request_bytes = 10;
    assert_eq!(
        expect_open_error(&client, &HttpRequest::get("http://127.0.0.1:1"), &bounds),
        before(TransportFailure::TooLarge)
    );
    for request in [
        HttpRequest::get("https://user:secret@example.test/"),
        HttpRequest::get("file:///tmp/a"),
        HttpRequest::get("http://127.0.0.1:1/").header("X-A", "value\r\ninjected: yes"),
        HttpRequest::get("http://127.0.0.1:1/").header("Content-Length", "9"),
    ] {
        assert_eq!(
            expect_open_error(&client, &request, &limits()),
            before(TransportFailure::InvalidRequest)
        );
    }
    let cancellation = Cancellation::new();
    cancellation.cancel();
    assert!(matches!(
        client.open(
            &HttpRequest::get("http://127.0.0.1:1"),
            &limits(),
            &cancellation
        ),
        Err(TransportError {
            phase: SendPhase::NotSent,
            kind: TransportFailure::Cancelled
        })
    ));
}
#[test]
fn http_header_and_body_bounds_fail_without_truncating_or_leaking() {
    for response in [
        b"HTTP/1.1 200 OK\r\nX-Large: abcdefghijklmnopqrstuvwxyz\r\nContent-Length: 0\r\n\r\n"
            .as_slice(),
        b"HTTP/1.1 200 OK\r\nX-A: a\r\nX-B: b\r\n\r\n",
    ] {
        let response = response.to_vec();
        let server = Server::new(move |mut socket| {
            request_bytes(&mut socket);
            socket.write_all(&response).unwrap();
        });
        let mut bounds = limits();
        bounds.max_header_bytes = 32;
        bounds.max_headers = 1;
        assert_eq!(
            expect_open_error(
                &NativeStreamingHttp::direct().unwrap(),
                &HttpRequest::get(server.url()),
                &bounds
            ),
            after(TransportFailure::TooLarge)
        );
        server.finish();
    }
    let server = Server::new(|mut socket| {
        request_bytes(&mut socket);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n12345\r\n0\r\n\r\n",
            )
            .unwrap();
    });
    let mut bounds = limits();
    bounds.max_response_bytes = 4;
    let mut stream = NativeStreamingHttp::direct()
        .unwrap()
        .open(
            &HttpRequest::get(server.url()),
            &bounds,
            &Cancellation::new(),
        )
        .unwrap();
    assert_eq!(all(&mut *stream), Err(after(TransportFailure::TooLarge)));
    assert_eq!(
        stream.read(&mut [0; 8], &Cancellation::new()),
        Err(after(TransportFailure::TooLarge))
    );
    server.finish();
}
#[test]
fn http_tls_verifies_roots_and_close_interrupts_a_stalled_tls_body() {
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der().clone()).unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der()).into(),
    )
    .unwrap();
    let tls = Arc::new(tls);
    let server = Server::new(move |socket| {
        let mut socket =
            rustls::StreamOwned::new(rustls::ServerConnection::new(tls).unwrap(), socket);
        let bytes = String::from_utf8(request_bytes(&mut socket)).unwrap();
        assert!(bytes.contains("authorization: tls-secret\r\n"));
        disconnected_after_head(socket);
    });
    let client = NativeStreamingHttp::with_roots(roots).unwrap();
    let mut stream = client
        .open(
            &HttpRequest::get(format!("https://localhost:{}/rpc", server.address.port()))
                .secret_header(
                    "Authorization",
                    devworks_core::ports::secrets::Secret::new("tls-secret"),
                ),
            &limits(),
            &Cancellation::new(),
        )
        .unwrap();
    let closer = stream.closer();
    let (result, received) = mpsc::channel();
    let reader = thread::spawn(move || {
        result
            .send(stream.read(&mut [0; 8], &Cancellation::new()))
            .unwrap();
    });
    assert_eq!(
        received.recv_timeout(Duration::from_millis(20)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    closer.close().unwrap();
    assert_eq!(
        received.recv_timeout(Duration::from_secs(1)).unwrap(),
        Err(after(TransportFailure::Cancelled))
    );
    reader.join().unwrap();
    server.finish();
}
#[test]
fn http_environment_proxy_is_preserved_in_a_scoped_child_process() {
    let server = Server::new(|mut socket| {
        let bytes = String::from_utf8(request_bytes(&mut socket)).unwrap();
        assert!(bytes.starts_with("POST http://provider.invalid/rpc HTTP/1.1\r\n"));
        assert!(bytes.contains("authorization: provider-secret\r\n"));
        assert!(bytes.contains("proxy-authorization: Basic dTpw\r\n"));
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .unwrap();
    });
    let proxy = format!("http://u:p@{}", server.address);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "provider_stream::tests::proxy_child",
            "--nocapture",
        ])
        .env("DW_STREAM_PROXY_CHILD", "1")
        .env("HTTP_PROXY", &proxy)
        .env("http_proxy", &proxy)
        .env("ALL_PROXY", &proxy)
        .env("all_proxy", &proxy)
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .status()
        .unwrap();
    assert!(status.success());
    server.finish();
}
#[test]
fn proxy_child() {
    if std::env::var_os("DW_STREAM_PROXY_CHILD").is_none() {
        return;
    }
    let mut stream = NativeStreamingHttp::new()
        .unwrap()
        .open(
            &HttpRequest::post(
                "http://provider.invalid/rpc",
                "application/json",
                b"{}".to_vec(),
            )
            .header("Authorization", "provider-secret"),
            &limits(),
            &Cancellation::new(),
        )
        .unwrap();
    assert_eq!(all(&mut *stream).unwrap(), b"ok");
}

#[test]
fn http_header_wait_cancellation_and_idle_are_bounded_after_send() {
    for cancel in [true, false] {
        let (observed, request_seen) = mpsc::channel();
        let server = Server::new(move |mut socket| {
            request_bytes(&mut socket);
            observed.send(()).unwrap();
            let mut byte = [0];
            assert_eq!(socket.read(&mut byte).unwrap(), 0);
        });
        let mut bounds = limits();
        bounds.idle_timeout_ms = 80;
        let client = NativeStreamingHttp::direct().unwrap();
        let cancellation = Cancellation::new();
        let trigger = cancellation.clone();
        let request = HttpRequest::get(server.url());
        let opened = thread::spawn(move || {
            expect_open_error_cancel(&client, &request, &bounds, &cancellation)
        });
        request_seen.recv_timeout(Duration::from_secs(1)).unwrap();
        if cancel {
            trigger.cancel();
        }
        assert_eq!(
            opened.join().unwrap(),
            after(if cancel {
                TransportFailure::Cancelled
            } else {
                TransportFailure::IdleTimeout
            })
        );
        server.finish();
    }
}
fn expect_open_error_cancel(
    client: &NativeStreamingHttp,
    request: &HttpRequest,
    bounds: &StreamLimits,
    cancellation: &Cancellation,
) -> TransportError {
    match client.open(request, bounds, cancellation) {
        Err(error) => error,
        Ok(_) => panic!("expected interruption"),
    }
}
#[test]
fn http_truncated_body_and_invalid_chunking_are_protocol_failures() {
    for bytes in [
        b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nshort".as_slice(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nnot-hex\r\n",
    ] {
        let bytes = bytes.to_vec();
        let server = Server::new(move |mut socket| {
            request_bytes(&mut socket);
            socket.write_all(&bytes).unwrap();
        });
        let mut stream = NativeStreamingHttp::direct()
            .unwrap()
            .open(
                &HttpRequest::get(server.url()),
                &limits(),
                &Cancellation::new(),
            )
            .unwrap();
        assert_eq!(all(&mut *stream), Err(after(TransportFailure::Protocol)));
        server.finish();
    }
}
#[test]
fn http_untrusted_tls_certificate_is_not_sent_and_secrets_never_reach_peer() {
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(key_pair.serialize_der()).into(),
    )
    .unwrap();
    let server = Server::new(move |socket| {
        let mut socket = rustls::StreamOwned::new(
            rustls::ServerConnection::new(Arc::new(tls)).unwrap(),
            socket,
        );
        let mut bytes = [0; 1024];
        assert!(socket.read(&mut bytes).is_err());
        assert!(!String::from_utf8_lossy(&bytes).contains("do-not-send"));
    });
    let client = NativeStreamingHttp::direct().unwrap();
    let request = HttpRequest::get(format!("https://localhost:{}/rpc", server.address.port()))
        .secret_header(
            "Authorization",
            devworks_core::ports::secrets::Secret::new("do-not-send"),
        );
    assert_eq!(
        expect_open_error(&client, &request, &limits()),
        before(TransportFailure::Unreachable)
    );
    server.finish();
}

#[test]
fn http_exposes_a_chunk_before_the_server_finishes_the_response() {
    let (continue_body, permitted) = mpsc::channel();
    let server = Server::new(move |mut socket| {
        request_bytes(&mut socket);
        socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\n\r\n1\r\na\r\n").unwrap();
        socket.flush().unwrap();
        permitted.recv_timeout(Duration::from_secs(1)).unwrap();
        socket.write_all(b"1\r\nb\r\n0\r\n\r\n").unwrap();
    });
    let mut stream = NativeStreamingHttp::direct()
        .unwrap()
        .open(
            &HttpRequest::get(server.url()),
            &limits(),
            &Cancellation::new(),
        )
        .unwrap();
    let mut byte = [0];
    assert_eq!(stream.read(&mut byte, &Cancellation::new()).unwrap(), 1);
    assert_eq!(byte, *b"a");
    continue_body.send(()).unwrap();
    assert_eq!(all(&mut *stream).unwrap(), b"b");
    server.finish();
}

#[test]
fn http_library_parser_is_bounded_before_tighter_semantic_head_validation() {
    for oversized_header in [true, false] {
        let server = Server::new(move |mut socket| {
            request_bytes(&mut socket);
            let mut head = String::from("HTTP/1.1 200 OK\r\n");
            if oversized_header {
                head.push_str(&format!("X-Large: {}\r\n", "x".repeat(2 * 1024 * 1024)));
            } else {
                for i in 0..101 {
                    head.push_str(&format!("X-{i}: x\r\n"));
                }
            }
            head.push_str("\r\n");
            let _ = socket.write_all(head.as_bytes()); // Expected peer rejection can interrupt write.
        });
        let mut bounds = limits();
        bounds.max_header_bytes = 1024 * 1024;
        bounds.max_headers = 1000;
        assert_eq!(
            expect_open_error(
                &NativeStreamingHttp::direct().unwrap(),
                &HttpRequest::get(server.url()),
                &bounds
            ),
            after(TransportFailure::TooLarge)
        );
        server.finish();
    }
}
#[test]
fn http_invalid_request_never_connects_and_refused_connection_is_not_sent() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let client = NativeStreamingHttp::direct().unwrap();
    let request = HttpRequest::get(format!("http://{address}/rpc")).header("Bad Header", "value");
    assert_eq!(
        expect_open_error(&client, &request, &limits()),
        before(TransportFailure::InvalidRequest)
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    drop(listener);
    assert_eq!(
        expect_open_error(
            &client,
            &HttpRequest::get(format!("http://{address}/rpc")),
            &limits()
        ),
        before(TransportFailure::Unreachable)
    );
}
