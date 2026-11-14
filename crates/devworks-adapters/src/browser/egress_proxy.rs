//! The egress proxy of one owned-browser session: an HTTP proxy on a loopback port that the
//! browser is launched to send every connection through (`--proxy-server`, loopback not
//! bypassed).
//!
//! For each connection the browser asks for (`CONNECT host:port` for HTTPS and WebSockets, an
//! absolute `http://` request otherwise) the proxy asks the core egress policy
//! ([`ConnectPolicy`]) once for the admitted addresses of that host and port, and connects only
//! to one of those addresses. The browser never resolves a name it connects to, and the proxy
//! never resolves twice, so the address that was checked is the address that is reached: a DNS
//! answer that changes after the check (rebinding) is never used. A refused destination gets
//! `403`, an unreachable one `502`, and the proxy never connects to itself.
//!
//! A plain HTTP request is forwarded alone (`Connection: close` both ways), so a later request on
//! the same browser connection can never reach the first request's server. A tunnel is relayed
//! byte for byte (TLS and WebSocket frames stay end to end).
use devworks_core::ports::browser::{BrowserFault, ConnectPolicy, RunningProxy};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// Largest request or response head the proxy reads.
pub const HEAD_LIMIT: usize = 64 * 1024;
/// Longest the browser may take to send a request head.
pub const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest one connection attempt to an admitted address may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest a write to either side may stall.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
/// Most connections relayed at once.
pub const MAX_CONNECTIONS: usize = 256;
const MAX_HEADERS: usize = 128;

/// Opens a connection to one admitted address within a bound.
pub type Dial = Arc<dyn Fn(SocketAddr, Duration) -> io::Result<TcpStream> + Send + Sync>;

/// A running proxy; dropping it stops accepting connections.
pub struct EgressProxy {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
}

impl EgressProxy {
    /// Starts a proxy that admits connections through `policy`.
    pub fn start(policy: Arc<dyn ConnectPolicy>) -> Result<Self, BrowserFault> {
        Self::start_with(
            policy,
            Arc::new(|to, bound| TcpStream::connect_timeout(&to, bound)),
        )
    }

    /// As [`Self::start`], connecting to admitted addresses through `dial`.
    pub fn start_with(policy: Arc<dyn ConnectPolicy>, dial: Dial) -> Result<Self, BrowserFault> {
        let fault = |e: io::Error| BrowserFault::Io(format!("egress proxy: {e}"));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(fault)?;
        let address = listener.local_addr().map_err(fault)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let open = Arc::new(AtomicUsize::new(0));
        std::thread::Builder::new()
            .name("browser-egress-proxy".into())
            .spawn(move || {
                for client in listener.incoming() {
                    if stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(client) = client else {
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    };
                    let held = Held::new(open.clone());
                    if held.count > MAX_CONNECTIONS {
                        let _ = client.set_write_timeout(Some(Duration::from_secs(1)));
                        respond(&client, "503 Service Unavailable");
                        continue;
                    }
                    let (policy, dial) = (policy.clone(), dial.clone());
                    // A thread that cannot be started drops the connection (and its count).
                    let _ = std::thread::Builder::new()
                        .name("browser-egress-connection".into())
                        .spawn(move || {
                            let _held = held;
                            serve(client, &*policy, &dial, address);
                        });
                }
            })
            .map_err(fault)?;
        Ok(Self { address, stop })
    }
}

impl RunningProxy for EgressProxy {
    fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes the accept loop so it sees the stop and closes the listener.
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(1));
    }
}

/// One counted open connection.
struct Held {
    open: Arc<AtomicUsize>,
    count: usize,
}
impl Held {
    fn new(open: Arc<AtomicUsize>) -> Self {
        let count = open.fetch_add(1, Ordering::SeqCst) + 1;
        Self { open, count }
    }
}
impl Drop for Held {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What the browser asked for.
#[derive(Debug, PartialEq, Eq)]
struct Asked {
    host: String,
    port: u16,
    /// The head to send upstream for a plain HTTP request; `None` for a tunnel.
    forward: Option<Vec<u8>>,
}

fn respond(mut client: &TcpStream, status: &str) {
    let _ = write!(
        client,
        "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
}

fn serve(client: TcpStream, policy: &dyn ConnectPolicy, dial: &Dial, own: SocketAddr) {
    let _ = client.set_read_timeout(Some(HEAD_TIMEOUT));
    let _ = client.set_write_timeout(Some(WRITE_TIMEOUT));
    let Some((head, rest)) = read_head(&client) else {
        return respond(&client, "400 Bad Request");
    };
    let Some(asked) = parse_request(&head) else {
        return respond(&client, "400 Bad Request");
    };
    // The one check of this connection: its answer is the only place it may go.
    let Ok(admitted) = policy.addresses(&asked.host, asked.port) else {
        return respond(&client, "403 Forbidden");
    };
    let candidates: Vec<SocketAddr> = admitted
        .into_iter()
        .map(|ip| SocketAddr::new(ip, asked.port))
        // Never the proxy itself (a loop through its own port).
        .filter(|a| !(a.ip().is_loopback() && a.port() == own.port()))
        .collect();
    if candidates.is_empty() {
        return respond(&client, "403 Forbidden");
    }
    let Some(upstream) = candidates
        .into_iter()
        .find_map(|a| dial(a, CONNECT_TIMEOUT).ok())
    else {
        return respond(&client, "502 Bad Gateway");
    };
    let _ = upstream.set_write_timeout(Some(WRITE_TIMEOUT));
    let sent = match &asked.forward {
        None => (&client)
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .and_then(|()| (&upstream).write_all(&rest)),
        Some(head) => (&upstream)
            .write_all(head)
            .and_then(|()| (&upstream).write_all(&rest)),
    };
    if sent.is_ok() {
        relay(&client, &upstream, asked.forward.is_some());
    }
    let _ = client.shutdown(Shutdown::Both);
    let _ = upstream.shutdown(Shutdown::Both);
}

/// Reads a head (through its blank line) within [`HEAD_LIMIT`]: the head and the bytes read
/// after it.
fn read_head(stream: &TcpStream) -> Option<(Vec<u8>, Vec<u8>)> {
    read_head_after(stream, Vec::new())
}

/// As [`read_head`], with `buf` already read.
fn read_head_after(mut stream: &TcpStream, mut buf: Vec<u8>) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(end + 4);
            return Some((buf, rest));
        }
        if buf.len() > HEAD_LIMIT {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Headers that belong to one hop and are never forwarded.
fn hop_by_hop(name: &str) -> bool {
    [
        "connection",
        "proxy-connection",
        "keep-alive",
        "proxy-authorization",
        "proxy-authenticate",
        "te",
        "trailer",
        "upgrade",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

fn parse_request(head: &[u8]) -> Option<Asked> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut req = httparse::Request::new(&mut headers);
    if !matches!(req.parse(head), Ok(httparse::Status::Complete(_))) {
        return None;
    }
    let (method, target) = (req.method?, req.path?);
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = authority(target, None)?;
        return Some(Asked {
            host,
            port,
            forward: None,
        });
    }
    // Only absolute `http://` requests are proxied (HTTPS and WebSockets arrive as CONNECT).
    let rest = target
        .get(..7)
        .filter(|s| s.eq_ignore_ascii_case("http://"))
        .map(|_| &target[7..])?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (auth, path) = rest.split_at(end);
    let (host, port) = authority(auth, Some(80))?;
    let path = path.split('#').next().unwrap_or("");
    let path = match path.chars().next() {
        None => "/".to_owned(),
        Some('?') => format!("/{path}"),
        Some(_) => path.to_owned(),
    };
    let mut out = format!("{method} {path} HTTP/1.1\r\n").into_bytes();
    for h in req.headers.iter().filter(|h| !hop_by_hop(h.name)) {
        out.extend_from_slice(h.name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(h.value);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    Some(Asked {
        host,
        port,
        forward: Some(out),
    })
}

/// Splits `host:port` (IPv6 in brackets); without a port, `default` (none: required).
fn authority(s: &str, default: Option<u16>) -> Option<(String, u16)> {
    let (host, port) = match s.strip_prefix('[') {
        Some(inner) => {
            let close = inner.find(']')?;
            let after = &inner[close + 1..];
            let port = match after {
                "" => None,
                p => Some(p.strip_prefix(':')?),
            };
            (&s[..close + 2], port)
        }
        None => match s.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (s, None),
        },
    };
    let port = match port {
        Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0)?,
        None => default?,
    };
    (!host.is_empty() && !host.contains('@')).then(|| (host.to_owned(), port))
}

/// Relays both directions until either ends. For a plain HTTP request the response head is
/// rewritten to close the connection after it.
fn relay(client: &TcpStream, upstream: &TcpStream, plain: bool) {
    let (Ok(c), Ok(u)) = (client.try_clone(), upstream.try_clone()) else {
        return;
    };
    let _ = client.set_read_timeout(None);
    let up = std::thread::Builder::new()
        .name("browser-egress-upload".into())
        .spawn(move || {
            let _ = io::copy(&mut &c, &mut &u);
            let _ = c.shutdown(Shutdown::Both);
            let _ = u.shutdown(Shutdown::Both);
        });
    if !plain || forward_response_head(upstream, client) {
        let _ = io::copy(&mut &*upstream, &mut &*client);
    }
    let _ = client.shutdown(Shutdown::Both);
    let _ = upstream.shutdown(Shutdown::Both);
    if let Ok(up) = up {
        let _ = up.join();
    }
}

/// Forwards the response head(s) of a plain HTTP request with `Connection: close`, so the
/// browser never sends another request on this connection. Interim (1xx) heads pass as they
/// are. `false` when no valid head arrived.
fn forward_response_head(upstream: &TcpStream, mut client: &TcpStream) -> bool {
    let mut pending = Vec::new();
    loop {
        let Some((head, rest)) = read_head_after(upstream, std::mem::take(&mut pending)) else {
            return false;
        };
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut resp = httparse::Response::new(&mut headers);
        let Ok(httparse::Status::Complete(_)) = resp.parse(&head) else {
            return false;
        };
        let Some(code) = resp.code else {
            return false;
        };
        if (100..200).contains(&code) && code != 101 {
            if client.write_all(&head).is_err() {
                return false;
            }
            pending = rest;
            continue;
        }
        let line_end = head.windows(2).position(|w| w == b"\r\n").unwrap_or(0);
        let mut out = head[..line_end + 2].to_vec();
        for h in resp.headers.iter().filter(|h| !hop_by_hop(h.name)) {
            out.extend_from_slice(h.name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(h.value);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"Connection: close\r\nProxy-Connection: close\r\n\r\n");
        return client.write_all(&out).is_ok() && client.write_all(&rest).is_ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_core::services::browser::egress::addresses;
    use std::collections::VecDeque;
    use std::net::IpAddr;
    use std::sync::Mutex;

    /// The core policy over a scripted resolver: each name answers from its queue in turn (the
    /// last answer repeats); every lookup is recorded.
    struct Scripted {
        answers: Mutex<Vec<(String, VecDeque<Vec<IpAddr>>)>>,
        asked: Mutex<Vec<String>>,
        allow_private: bool,
    }
    impl Scripted {
        fn new(allow_private: bool, answers: &[(&str, &[&[&str]])]) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(
                    answers
                        .iter()
                        .map(|(name, seq)| {
                            let seq = seq
                                .iter()
                                .map(|ips| ips.iter().map(|s| s.parse().unwrap()).collect())
                                .collect();
                            ((*name).to_owned(), seq)
                        })
                        .collect(),
                ),
                asked: Mutex::new(vec![]),
                allow_private,
            })
        }
        fn lookup(&self, host: &str) -> Result<Vec<IpAddr>, String> {
            self.asked.lock().unwrap().push(host.to_owned());
            let mut answers = self.answers.lock().unwrap();
            let (_, seq) = answers
                .iter_mut()
                .find(|(n, _)| n == host)
                .ok_or("no such name")?;
            Ok(if seq.len() > 1 {
                seq.pop_front().unwrap()
            } else {
                seq[0].clone()
            })
        }
    }
    impl ConnectPolicy for Scripted {
        fn addresses(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
            addresses(host, port, &|| self.allow_private, &mut |h, _| {
                self.lookup(h)
            })
            .map_err(|r| r.detail)
        }
    }

    /// Dials through a table: addresses mapped to local listeners, everything else refused.
    /// Every attempt is recorded.
    fn dialer(map: Vec<(SocketAddr, SocketAddr)>) -> (Dial, Arc<Mutex<Vec<SocketAddr>>>) {
        let tried = Arc::new(Mutex::new(Vec::new()));
        let log = tried.clone();
        let dial: Dial = Arc::new(move |to: SocketAddr, bound| {
            log.lock().unwrap().push(to);
            match map.iter().find(|(from, _)| *from == to) {
                Some((_, local)) => TcpStream::connect_timeout(local, bound),
                None => Err(io::Error::new(io::ErrorKind::ConnectionRefused, "unmapped")),
            }
        });
        (dial, tried)
    }

    /// A server that records the head it receives and answers with `reply`.
    fn server(reply: &'static [u8]) -> (SocketAddr, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let (head, _) = read_head(&s).unwrap_or_default();
            s.write_all(reply).unwrap();
            head
        });
        (addr, handle)
    }

    /// Sends `request` through the proxy and reads its answer until the proxy closes.
    fn ask(proxy: &EgressProxy, request: &str) -> String {
        let mut s = TcpStream::connect(proxy.address()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        String::from_utf8_lossy(&out).into_owned()
    }

    fn public() -> SocketAddr {
        "93.184.216.34:80".parse().unwrap()
    }

    #[test]
    fn a_tunnel_or_request_to_a_refused_address_is_refused_before_any_connection() {
        let policy = Scripted::new(
            false,
            &[
                ("lan.example", &[&["192.168.1.10"]]),
                ("mixed.example", &[&["93.184.216.34", "169.254.169.254"]]),
            ],
        );
        let (dial, tried) = dialer(vec![]);
        let proxy = EgressProxy::start_with(policy, dial).unwrap();
        // WebSocket (ws and wss alike) and HTTPS connections arrive as CONNECT tunnels.
        for target in [
            "169.254.169.254:80",
            "169.254.169.254:443",
            "[fd00:ec2::254]:443",
            "metadata.google.internal:80",
            "[fe80::1]:443",
            "192.168.1.1:443",
            "10.0.0.5:8080",
            "lan.example:443",
            "mixed.example:443",
            "0.0.0.0:80",
        ] {
            let answer = ask(
                &proxy,
                &format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n"),
            );
            assert!(answer.starts_with("HTTP/1.1 403 "), "{target}: {answer}");
        }
        let answer = ask(
            &proxy,
            "GET http://169.254.169.254/latest/meta-data/ HTTP/1.1\r\nHost: 169.254.169.254\r\n\r\n",
        );
        assert!(answer.starts_with("HTTP/1.1 403 "), "{answer}");
        // Neither the proxy itself nor malformed requests are ever reached.
        let own = proxy.address();
        assert!(
            ask(&proxy, &format!("CONNECT {own} HTTP/1.1\r\n\r\n")).starts_with("HTTP/1.1 403 ")
        );
        for bad in [
            "GET /relative HTTP/1.1\r\n\r\n",
            "GET ftp://example.com/ HTTP/1.1\r\n\r\n",
            "CONNECT example.com HTTP/1.1\r\n\r\n",
            "CONNECT user@example.com:443 HTTP/1.1\r\n\r\n",
        ] {
            assert!(ask(&proxy, bad).starts_with("HTTP/1.1 400 "), "{bad}");
        }
        assert!(tried.lock().unwrap().is_empty(), "nothing was dialed");
    }

    #[test]
    fn a_websocket_tunnel_to_an_admitted_address_carries_the_upgrade_end_to_end() {
        let (local, server) = server(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n");
        let (dial, tried) = dialer(vec![(local, local)]);
        let proxy = EgressProxy::start_with(Scripted::new(false, &[]), dial).unwrap();
        let mut s = TcpStream::connect(proxy.address()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(s, "CONNECT {local} HTTP/1.1\r\nHost: {local}\r\n\r\n").unwrap();
        let (head, _) = read_head(&s).unwrap();
        assert!(head.starts_with(b"HTTP/1.1 200 "));
        s.write_all(
            b"GET /socket HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n",
        )
        .unwrap();
        let (head, _) = read_head(&s).unwrap();
        assert!(head.starts_with(b"HTTP/1.1 101 "));
        let seen = String::from_utf8(server.join().unwrap()).unwrap();
        assert!(seen.contains("Upgrade: websocket"), "{seen}");
        assert_eq!(*tried.lock().unwrap(), vec![local]);
    }

    #[test]
    fn a_name_is_resolved_once_per_connection_and_only_its_checked_answer_is_reached() {
        // `rebind.example` answers a public address once, then the metadata address.
        let (local, server) =
            server(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nKeep-Alive: timeout=5\r\n\r\nok");
        let policy = Scripted::new(
            false,
            &[(
                "rebind.example",
                &[&["93.184.216.34"], &["169.254.169.254"]],
            )],
        );
        let metadata: SocketAddr = "169.254.169.254:80".parse().unwrap();
        let (dial, tried) = dialer(vec![(public(), local)]);
        let proxy = EgressProxy::start_with(policy.clone(), dial).unwrap();
        let answer = ask(
            &proxy,
            "GET http://rebind.example/a?b=1#frag HTTP/1.1\r\nHost: rebind.example\r\nProxy-Connection: keep-alive\r\nAccept: */*\r\n\r\n",
        );
        // The connection went to the address that was checked, and the browser is told to
        // close it after this response.
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
        assert!(answer.contains("Connection: close\r\n"), "{answer}");
        assert!(!answer.contains("Keep-Alive"), "{answer}");
        assert!(answer.ends_with("\r\n\r\nok"), "{answer}");
        let seen = String::from_utf8(server.join().unwrap()).unwrap();
        assert!(seen.starts_with("GET /a?b=1 HTTP/1.1\r\n"), "{seen}");
        assert!(seen.contains("Host: rebind.example\r\n"), "{seen}");
        assert!(seen.contains("Connection: close\r\n"), "{seen}");
        assert!(!seen.contains("Proxy-Connection"), "{seen}");
        assert_eq!(*policy.asked.lock().unwrap(), vec!["rebind.example"]);
        assert_eq!(*tried.lock().unwrap(), vec![public()]);
        // The name now answers the metadata address: the next connection is refused, and the
        // metadata address is never dialed.
        let answer = ask(&proxy, "CONNECT rebind.example:80 HTTP/1.1\r\n\r\n");
        assert!(answer.starts_with("HTTP/1.1 403 "), "{answer}");
        assert!(!tried.lock().unwrap().contains(&metadata));
    }

    #[test]
    fn a_name_checked_public_earlier_and_rebound_to_metadata_at_connect_time_is_refused() {
        let policy = Scripted::new(
            false,
            &[(
                "rebind.example",
                &[&["93.184.216.34"], &["169.254.169.254"]],
            )],
        );
        // The check before the request (a navigation's own check) sees the public answer.
        assert_eq!(
            policy.addresses("rebind.example", 443),
            Ok(vec!["93.184.216.34".parse().unwrap()])
        );
        let (dial, tried) = dialer(vec![]);
        let proxy = EgressProxy::start_with(policy.clone(), dial).unwrap();
        // When the browser connects, the name answers the metadata address: refused, and
        // nothing is dialed.
        let answer = ask(&proxy, "CONNECT rebind.example:443 HTTP/1.1\r\n\r\n");
        assert!(answer.starts_with("HTTP/1.1 403 "), "{answer}");
        assert!(tried.lock().unwrap().is_empty());
        assert_eq!(policy.asked.lock().unwrap().len(), 2);
    }

    #[test]
    fn an_unreachable_admitted_address_is_a_bad_gateway_and_a_stopped_proxy_accepts_nothing() {
        let (dial, tried) = dialer(vec![]);
        let proxy = EgressProxy::start_with(Scripted::new(false, &[]), dial).unwrap();
        let answer = ask(&proxy, "CONNECT 8.8.8.8:443 HTTP/1.1\r\n\r\n");
        assert!(answer.starts_with("HTTP/1.1 502 "), "{answer}");
        assert_eq!(*tried.lock().unwrap(), vec!["8.8.8.8:443".parse().unwrap()]);
        let address = proxy.address();
        drop(proxy);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "the proxy still accepts"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn authorities_and_absolute_targets_are_split_exactly() {
        assert_eq!(authority("[::1]:8080", None), Some(("[::1]".into(), 8080)));
        assert_eq!(authority("a.test", Some(80)), Some(("a.test".into(), 80)));
        for bad in [
            "a.test",
            "a.test:0",
            "a.test:x",
            "[::1]x",
            "[::1",
            ":80",
            "u@a.test:80",
        ] {
            assert_eq!(authority(bad, None), None, "{bad}");
        }
        let asked =
            parse_request(b"GET http://a.test:81?q HTTP/1.1\r\nHost: a.test:81\r\n\r\n").unwrap();
        assert_eq!((asked.host.as_str(), asked.port), ("a.test", 81));
        assert!(asked.forward.unwrap().starts_with(b"GET /?q HTTP/1.1\r\n"));
    }
}
