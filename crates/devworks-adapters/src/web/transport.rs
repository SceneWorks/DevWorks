//! The native HTTP/1.1 transport of the web tools: one `GET` per hop over a TCP connection to the
//! exact address core checked, with TLS by rustls (ring provider) against Mozilla's bundled root
//! set.
//!
//! What it never does: resolve a name (it is handed a socket address), follow a redirect, keep
//! or send a cookie, add an authorization or proxy header, reuse a connection, or accept a
//! compressed body it did not ask for (`Accept-Encoding: identity`). What it bounds: the
//! connection, handshake, write and every body read end at the hop deadline; the response head
//! is at most [`HEAD_LIMIT`] bytes and [`HEADER_LIMIT`] fields; chunk-size and trailer lines are
//! bounded too. Cancellation is checked between waits of at most [`WAIT`].
//!
//! Faults say whether request bytes may have reached the server: everything up to and including
//! the TLS handshake is `NotSent`; from the first byte of the request on, a failure is
//! `AfterSend`.
use devworks_core::ports::storage::Cancellation;
use devworks_core::ports::web::{
    HopRequest, HopResponse, HopTransport, NetFault, Scheme, ServerName,
};
use rustls::pki_types::{IpAddr as TlsIp, ServerName as TlsName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use std::io::{self, Cursor, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Largest response head accepted.
pub const HEAD_LIMIT: usize = 64 * 1024;
/// Most header fields accepted.
pub const HEADER_LIMIT: usize = 100;
/// Longest chunk-size or trailer line.
const LINE_LIMIT: usize = 4 * 1024;
/// Longest single wait before cancellation is checked again.
pub const WAIT: Duration = Duration::from_millis(200);
/// Most interim (1xx) responses skipped before the final one.
const INTERIM_LIMIT: usize = 8;

/// The rustls-backed transport.
#[derive(Clone)]
pub struct NativeTransport {
    tls: Arc<ClientConfig>,
    connector: Connector,
}

/// Opens a TCP connection to an address within a bound (the system's `connect_timeout`; a test
/// seam can stand in for an unreachable or slow network).
pub type Connector = Arc<dyn Fn(SocketAddr, Duration) -> io::Result<TcpStream> + Send + Sync>;

impl Default for NativeTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeTransport {
    /// Trusts Mozilla's root set as bundled with this build (an explicit, reviewable trust
    /// policy rather than whatever the machine's store holds).
    pub fn new() -> Self {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Self::with_roots(roots)
    }

    /// Trusts exactly `roots` (tests and explicitly configured endpoints).
    pub fn with_roots(roots: RootCertStore) -> Self {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Self {
            tls: Arc::new(config),
            connector: Arc::new(|addr, wait| TcpStream::connect_timeout(&addr, wait)),
        }
    }

    /// The transport with another way to open connections (tests simulate refused, blackholed
    /// and slow networks with it; production uses the system connect).
    pub fn with_connector(mut self, connector: Connector) -> Self {
        self.connector = connector;
        self
    }
}

/// A plain or TLS connection.
enum Conn {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}
impl Conn {
    fn tcp(&self) -> &TcpStream {
        match self {
            Self::Plain(s) => s,
            Self::Tls(s) => &s.sock,
        }
    }
}
impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            Self::Tls(s) => s.read(buf),
        }
    }
}
impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            Self::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}
impl Drop for Conn {
    fn drop(&mut self) {
        let _ = self.tcp().shutdown(Shutdown::Both);
    }
}

/// The time left before `deadline`, never zero (a zero socket timeout is an error).
fn left(deadline: Instant) -> Option<Duration> {
    let d = deadline.saturating_duration_since(Instant::now());
    (!d.is_zero()).then(|| d.max(Duration::from_millis(1)))
}

fn cancelled(cancel: &[Cancellation]) -> bool {
    cancel.iter().any(Cancellation::is_cancelled)
}

fn timed_out(kind: ErrorKind) -> bool {
    // Unix reports an expired socket timeout as WouldBlock, Windows as TimedOut.
    matches!(kind, ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// The fields of a request line or header value may not carry a line break or NUL.
fn clean(s: &str) -> bool {
    !s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

impl HopTransport for NativeTransport {
    fn get(&self, request: &HopRequest<'_>) -> Result<HopResponse, NetFault> {
        let not_sent = |reason: String, timed_out: bool| NetFault::NotSent { reason, timed_out };
        if cancelled(&request.cancel) {
            return Err(NetFault::Cancelled { sent: false });
        }
        let fields_ok = clean(request.target)
            && request.target.starts_with('/')
            && !request.target.contains(' ')
            && clean(request.host_header)
            && clean(request.user_agent)
            && clean(request.accept)
            && request
                .headers
                .iter()
                .all(|(k, v)| clean(k) && clean(v) && !k.contains(':'));
        if !fields_ok {
            return Err(not_sent("the request has an invalid field".into(), false));
        }
        let tcp = self.connect(request)?;
        // The connection must be to the checked address, never anything a proxy chose.
        match tcp.peer_addr() {
            Ok(peer) if peer == request.addr => {}
            Ok(peer) => {
                return Err(not_sent(
                    format!("connected to {peer}, not the checked address"),
                    false,
                ));
            }
            Err(e) => return Err(not_sent(format!("connection lost: {e}"), false)),
        }
        let _ = tcp.set_nodelay(true);
        let mut conn = match request.scheme {
            Scheme::Http => Conn::Plain(tcp),
            Scheme::Https => self.handshake(tcp, request)?,
        };
        // From the first request byte on, a failure is after dispatch.
        let mut head = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\nAccept: {}\r\nAccept-Encoding: identity\r\nConnection: close\r\n",
            request.target, request.host_header, request.user_agent, request.accept
        );
        for (k, v) in request.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        write_all(&mut conn, head.as_bytes(), request)?;
        let (status, headers, rest) = read_head(&mut conn, request)?;
        let framing = framing(status, &headers)?;
        let body = Body {
            conn,
            pending: Cursor::new(rest),
            framing,
            deadline: request.deadline,
            cancel: request.cancel.clone(),
        };
        Ok(HopResponse {
            status,
            headers,
            body: Box::new(body),
        })
    }
}

impl NativeTransport {
    /// Connects to the checked address without blocking the caller past cancellation: the
    /// system connect runs on a helper thread bounded by the hop deadline, and the caller waits
    /// in slices of at most [`WAIT`]. Cancelled or timed out, the caller returns at once and the
    /// helper's late connection (if any) is closed unused, so nothing is ever sent on it.
    ///
    /// A refused connection (`ECONNREFUSED`, `WSAECONNREFUSED`: the peer answered with a reset)
    /// is not sent and not a timeout; a connection that runs out the deadline (`ETIMEDOUT`,
    /// `WSAETIMEDOUT`, or the deadline passing first) is a timeout. On Windows a refused
    /// connection is reported only after the stack's SYN retries (about two seconds), so it is
    /// classified as refused when that happens within the deadline and as a timeout otherwise.
    fn connect(&self, request: &HopRequest<'_>) -> Result<TcpStream, NetFault> {
        let not_sent = |reason: String, timed_out: bool| NetFault::NotSent { reason, timed_out };
        let Some(wait) = left(request.deadline) else {
            return Err(not_sent("the time limit passed".into(), true));
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let (connector, addr) = (self.connector.clone(), request.addr);
        std::thread::Builder::new()
            .name("devworks-web-connect".into())
            .spawn(move || {
                // A send after the caller gave up fails and drops (closes) the stream.
                let _ = tx.send(connector(addr, wait));
            })
            .map_err(|e| not_sent(format!("the connection could not start: {e}"), false))?;
        loop {
            if cancelled(&request.cancel) {
                return Err(NetFault::Cancelled { sent: false });
            }
            let Some(slice) = left(request.deadline) else {
                return Err(not_sent(
                    format!("connection to {addr} did not finish in time"),
                    true,
                ));
            };
            match rx.recv_timeout(slice.min(WAIT)) {
                Ok(Ok(tcp)) => return Ok(tcp),
                Ok(Err(e)) => {
                    return Err(not_sent(
                        format!("connection to {addr} failed: {e}"),
                        timed_out(e.kind()),
                    ));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(not_sent(format!("connection to {addr} failed"), false));
                }
            }
        }
    }

    fn handshake(&self, tcp: TcpStream, request: &HopRequest<'_>) -> Result<Conn, NetFault> {
        let not_sent = |reason: String, timed_out: bool| NetFault::NotSent { reason, timed_out };
        let name = match request.server_name {
            ServerName::Dns(d) => TlsName::try_from(d.clone())
                .map_err(|_| not_sent(format!("{d} is not a valid TLS server name"), false))?,
            ServerName::Ip(ip) => TlsName::IpAddress(TlsIp::from(*ip)),
        };
        let mut tls = ClientConnection::new(self.tls.clone(), name)
            .map_err(|e| not_sent(format!("TLS setup failed: {e}"), false))?;
        let mut sock = tcp;
        while tls.is_handshaking() {
            if cancelled(&request.cancel) {
                return Err(NetFault::Cancelled { sent: false });
            }
            let Some(wait) = left(request.deadline) else {
                return Err(not_sent(
                    "the TLS handshake did not finish in time".into(),
                    true,
                ));
            };
            let wait = Some(wait.min(WAIT));
            let _ = sock.set_read_timeout(wait);
            let _ = sock.set_write_timeout(wait);
            match tls.complete_io(&mut sock) {
                Ok(_) => {}
                Err(e) if timed_out(e.kind()) || e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(not_sent(format!("TLS handshake failed: {e}"), false)),
            }
        }
        Ok(Conn::Tls(Box::new(StreamOwned::new(tls, sock))))
    }
}

fn after(reason: String, timed_out: bool) -> NetFault {
    NetFault::AfterSend { reason, timed_out }
}

fn write_all(conn: &mut Conn, mut bytes: &[u8], request: &HopRequest<'_>) -> Result<(), NetFault> {
    while !bytes.is_empty() {
        if cancelled(&request.cancel) {
            return Err(NetFault::Cancelled { sent: true });
        }
        let Some(wait) = left(request.deadline) else {
            return Err(after(
                "the request could not be written in time".into(),
                true,
            ));
        };
        let _ = conn.tcp().set_write_timeout(Some(wait.min(WAIT)));
        match conn.write(bytes) {
            Ok(0) => return Err(after("the connection closed while writing".into(), false)),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if timed_out(e.kind()) || e.kind() == ErrorKind::Interrupted => {}
            Err(e) => {
                return Err(after(
                    format!("the request could not be written: {e}"),
                    false,
                ));
            }
        }
    }
    loop {
        match conn.flush() {
            Ok(()) => return Ok(()),
            Err(e) if timed_out(e.kind()) || e.kind() == ErrorKind::Interrupted => {
                if left(request.deadline).is_none() {
                    return Err(after(
                        "the request could not be written in time".into(),
                        true,
                    ));
                }
                if cancelled(&request.cancel) {
                    return Err(NetFault::Cancelled { sent: true });
                }
            }
            Err(e) => {
                return Err(after(
                    format!("the request could not be written: {e}"),
                    false,
                ));
            }
        }
    }
}

/// One bounded read with the remaining deadline: `Ok(0)` is end of stream.
fn read_some(
    conn: &mut Conn,
    buf: &mut [u8],
    deadline: Instant,
    cancel: &[Cancellation],
) -> io::Result<usize> {
    loop {
        if cancelled(cancel) {
            return Err(io::Error::new(ErrorKind::Interrupted, "cancelled"));
        }
        let Some(wait) = left(deadline) else {
            return Err(io::Error::new(ErrorKind::TimedOut, "the time limit passed"));
        };
        let _ = conn.tcp().set_read_timeout(Some(wait.min(WAIT)));
        match conn.read(buf) {
            Err(e) if timed_out(e.kind()) => continue,
            // A peer that closes without TLS close_notify ends the stream (common on the web);
            // framing still catches a truncated body.
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(0),
            other => return other,
        }
    }
}

fn io_fault(e: io::Error, what: &str) -> NetFault {
    match e.kind() {
        ErrorKind::Interrupted => NetFault::Cancelled { sent: true },
        ErrorKind::TimedOut => after(format!("{what}: the time limit passed"), true),
        _ => after(format!("{what}: {e}"), false),
    }
}

type Head = (u16, Vec<(String, String)>, Vec<u8>);

/// Reads the response head, skipping interim (1xx) responses.
fn read_head(conn: &mut Conn, request: &HopRequest<'_>) -> Result<Head, NetFault> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let mut interim = 0;
    loop {
        let mut fields = [httparse::EMPTY_HEADER; HEADER_LIMIT];
        let mut parsed = httparse::Response::new(&mut fields);
        match parsed.parse(&buf) {
            Ok(httparse::Status::Complete(len)) => {
                let status = parsed.code.unwrap_or(0);
                let headers: Vec<(String, String)> = parsed
                    .headers
                    .iter()
                    .map(|h| {
                        (
                            h.name.to_owned(),
                            String::from_utf8_lossy(h.value).trim().to_owned(),
                        )
                    })
                    .collect();
                let rest = buf[len..].to_vec();
                if (100..200).contains(&status) && status != 101 {
                    interim += 1;
                    if interim > INTERIM_LIMIT {
                        return Err(after("too many interim responses".into(), false));
                    }
                    buf = rest;
                    continue;
                }
                if !(200..600).contains(&status) {
                    return Err(after(format!("unsupported status {status}"), false));
                }
                return Ok((status, headers, rest));
            }
            Ok(httparse::Status::Partial) => {}
            Err(e) => return Err(after(format!("malformed response head: {e}"), false)),
        }
        if buf.len() >= HEAD_LIMIT {
            return Err(after("the response head is too large".into(), false));
        }
        let want = chunk.len().min(HEAD_LIMIT - buf.len());
        match read_some(conn, &mut chunk[..want], request.deadline, &request.cancel) {
            Ok(0) => {
                return Err(after(
                    "the connection closed before a response".into(),
                    false,
                ));
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(io_fault(e, "reading the response")),
        }
    }
}

/// How the body is delimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    Empty,
    Length(u64),
    Chunked(ChunkState),
    UntilClose,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChunkState {
    /// Expecting a chunk-size line.
    Size,
    /// Bytes left in the current chunk.
    Data(u64),
    /// The CRLF after a chunk's data.
    DataEnd,
    /// After the last chunk: trailer lines until an empty one.
    Trailers,
    Done,
}

fn framing(status: u16, headers: &[(String, String)]) -> Result<Framing, NetFault> {
    if status == 204 || status == 304 {
        return Ok(Framing::Empty);
    }
    let field = |name: &str| {
        headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
    };
    let encodings = field("transfer-encoding");
    if !encodings.is_empty() {
        let last = encodings
            .join(",")
            .rsplit(',')
            .next()
            .map(|s| s.trim().to_ascii_lowercase())
            .unwrap_or_default();
        return Ok(if last == "chunked" {
            Framing::Chunked(ChunkState::Size)
        } else {
            Framing::UntilClose
        });
    }
    let lengths = field("content-length");
    if lengths.is_empty() {
        return Ok(Framing::UntilClose);
    }
    let mut value: Option<u64> = None;
    for l in lengths.iter().flat_map(|l| l.split(',')) {
        let n: u64 = l
            .trim()
            .parse()
            .map_err(|_| after("an invalid Content-Length".into(), false))?;
        if value.is_some_and(|v| v != n) {
            return Err(after("conflicting Content-Length values".into(), false));
        }
        value = Some(n);
    }
    Ok(Framing::Length(value.unwrap_or(0)))
}

/// A decoded body over the connection.
struct Body {
    conn: Conn,
    /// Bytes read with the head that belong to the body.
    pending: Cursor<Vec<u8>>,
    framing: Framing,
    deadline: Instant,
    cancel: Vec<Cancellation>,
}

impl Body {
    fn raw(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.pending.read(buf)?;
        if n > 0 {
            return Ok(n);
        }
        read_some(&mut self.conn, buf, self.deadline, &self.cancel)
    }
    /// Reads one CRLF-terminated line (bounded), without the terminator.
    fn line(&mut self) -> io::Result<String> {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            if self.raw(&mut byte)? == 0 {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "the chunked body ended early",
                ));
            }
            if byte[0] == b'\n' {
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(String::from_utf8_lossy(&line).into_owned());
            }
            line.push(byte[0]);
            if line.len() > LINE_LIMIT {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "a chunk line is too long",
                ));
            }
        }
    }
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            match self.framing {
                Framing::Empty | Framing::Length(0) => return Ok(0),
                Framing::Length(left) => {
                    let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
                    let n = self.raw(&mut buf[..want])?;
                    if n == 0 {
                        return Err(io::Error::new(
                            ErrorKind::UnexpectedEof,
                            "the body ended before its Content-Length",
                        ));
                    }
                    self.framing = Framing::Length(left - n as u64);
                    return Ok(n);
                }
                Framing::UntilClose => return self.raw(buf),
                Framing::Chunked(state) => match state {
                    ChunkState::Done => return Ok(0),
                    ChunkState::Size => {
                        let line = self.line()?;
                        let hex = line.split(';').next().unwrap_or("").trim();
                        let size = u64::from_str_radix(hex, 16).map_err(|_| {
                            io::Error::new(ErrorKind::InvalidData, "an invalid chunk size")
                        })?;
                        self.framing = Framing::Chunked(if size == 0 {
                            ChunkState::Trailers
                        } else {
                            ChunkState::Data(size)
                        });
                    }
                    ChunkState::Data(left) => {
                        let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
                        let n = self.raw(&mut buf[..want])?;
                        if n == 0 {
                            return Err(io::Error::new(
                                ErrorKind::UnexpectedEof,
                                "the chunked body ended early",
                            ));
                        }
                        let rest = left - n as u64;
                        self.framing = Framing::Chunked(if rest == 0 {
                            ChunkState::DataEnd
                        } else {
                            ChunkState::Data(rest)
                        });
                        return Ok(n);
                    }
                    ChunkState::DataEnd => {
                        if !self.line()?.is_empty() {
                            return Err(io::Error::new(
                                ErrorKind::InvalidData,
                                "a chunk was longer than its size",
                            ));
                        }
                        self.framing = Framing::Chunked(ChunkState::Size);
                    }
                    ChunkState::Trailers => {
                        // Trailer fields are read and dropped (bounded by line count).
                        let mut lines = 0;
                        while !self.line()?.is_empty() {
                            lines += 1;
                            if lines > HEADER_LIMIT {
                                return Err(io::Error::new(
                                    ErrorKind::InvalidData,
                                    "too many trailer fields",
                                ));
                            }
                        }
                        self.framing = Framing::Chunked(ChunkState::Done);
                    }
                },
            }
        }
    }
}
