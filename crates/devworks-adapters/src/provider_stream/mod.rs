//! Provider HTTP streaming, independent of web-fetch egress permissions. Uses configured
//! endpoints and credentials already authorized by core; never follows a redirect or retries.
//!
//! HTTP/1 parsing is bounded by locked Hyper 1.12 (417792 bytes / 100 headers). Reqwest
//! exposes no smaller parser allocation knob: caller header limits are checked after parsing,
//! before publishing the head. Body handoff is demand-driven, with one bounded native chunk.
use devworks_core::ports::{
    http::{HeaderValue, HttpRequest, Method},
    provider_http::{
        HttpStream, ResponseHead, SendPhase, StreamCloser, StreamLimits, StreamingHttp,
        TransportError, TransportFailure,
    },
    storage::Cancellation,
};
use std::{
    fmt,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use tokio::{
    runtime::Handle,
    sync::{Notify, mpsc as async_mpsc, oneshot},
};

pub mod sse;
const READ_SIZE: usize = 16 * 1024;
const CANCEL_POLL: Duration = Duration::from_millis(10);
fn error(phase: SendPhase, kind: TransportFailure) -> TransportError {
    TransportError { phase, kind }
}
fn before(kind: TransportFailure) -> TransportError {
    error(SendPhase::NotSent, kind)
}
fn after(kind: TransportFailure) -> TransportError {
    error(SendPhase::AfterSend, kind)
}

// A single owned runtime thread, not a thread per DNS lookup/request. Dropping the last
// adapter/stream joins it even when the host happens to be inside another async runtime.
struct IoRuntime {
    handle: Handle,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}
impl IoRuntime {
    fn new() -> Result<Self, TransportError> {
        let (ready, started) = mpsc::sync_channel(1);
        let (stop, stopped) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("devworks-provider-http".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => {
                        if ready.send(Ok(runtime.handle().clone())).is_ok() {
                            runtime.block_on(async {
                                let _ = stopped.await;
                            });
                        }
                        // All response owners have quiesced before the last Arc reaches here.
                        // Shutdown also cancels and joins the client's connection/DNS tasks.
                        runtime.shutdown_timeout(Duration::from_secs(1));
                    }
                    Err(_) => {
                        let _ = ready.send(Err(before(TransportFailure::Unreachable)));
                    }
                }
            })
            .map_err(|_| before(TransportFailure::Unreachable))?;
        match started.recv() {
            Ok(Ok(handle)) => Ok(Self {
                handle,
                stop: Some(stop),
                thread: Some(thread),
            }),
            _ => {
                let _ = thread.join();
                Err(before(TransportFailure::Unreachable))
            }
        }
    }
}
impl Drop for IoRuntime {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone)]
pub struct NativeStreamingHttp {
    client: reqwest::Client,
    runtime: Arc<IoRuntime>,
}
impl fmt::Debug for NativeStreamingHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NativeStreamingHttp")
    }
}
impl NativeStreamingHttp {
    /// Production follows the user's HTTPS_PROXY/HTTP_PROXY/NO_PROXY environment, as NativeHttp does.
    pub fn new() -> Result<Self, TransportError> {
        Self::build(true, None)
    }
    /// Explicit direct transport for local fixtures and hosts that intentionally disable proxies.
    pub fn direct() -> Result<Self, TransportError> {
        Self::build(false, None)
    }
    /// Explicit trust roots, retaining certificate/name verification; no environment proxy.
    pub fn with_roots(roots: rustls::RootCertStore) -> Result<Self, TransportError> {
        Self::build(false, Some(roots))
    }
    fn build(
        env_proxy: bool,
        roots: Option<rustls::RootCertStore>,
    ) -> Result<Self, TransportError> {
        let runtime = Arc::new(IoRuntime::new()?);
        let roots = roots.unwrap_or_else(|| rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        });
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| before(TransportFailure::InvalidRequest))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let mut builder = reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .http1_only()
            .hickory_dns(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .referer(false)
            .pool_max_idle_per_host(0)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .user_agent("DevWorks");
        if !env_proxy {
            builder = builder.no_proxy();
        }
        let client = {
            let _entered = runtime.handle.enter();
            builder.build()
        }
        .map_err(|_| before(TransportFailure::InvalidRequest))?;
        Ok(Self { client, runtime })
    }
    fn prepare(
        &self,
        request: &HttpRequest,
        limits: &StreamLimits,
    ) -> Result<reqwest::Request, TransportError> {
        if !limits.validate() || request.timeout_ms == 0 || request.max_response_bytes == 0 {
            return Err(before(TransportFailure::InvalidRequest));
        }
        let url =
            url::Url::parse(&request.url).map_err(|_| before(TransportFailure::InvalidRequest))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(before(TransportFailure::InvalidRequest));
        }
        // Upper bound includes absolute URI (proxy request form), generated framing/host fields,
        // and method/version. Reject before cloning a body or materializing header values.
        let mut bytes = url
            .as_str()
            .len()
            .checked_add(256)
            .ok_or_else(|| before(TransportFailure::TooLarge))?;
        bytes = bytes
            .checked_add(request.body.as_ref().map_or(0, Vec::len))
            .ok_or_else(|| before(TransportFailure::TooLarge))?;
        for (name, value) in &request.headers {
            bytes = bytes
                .checked_add(name.len())
                .and_then(|n| n.checked_add(value.as_str().len()))
                .and_then(|n| n.checked_add(4))
                .ok_or_else(|| before(TransportFailure::TooLarge))?;
        }
        if bytes > limits.max_request_bytes {
            return Err(before(TransportFailure::TooLarge));
        }
        let method = match request.method {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Patch => reqwest::Method::PATCH,
        };
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &request.headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| before(TransportFailure::InvalidRequest))?;
            // Framing is exclusively the client's responsibility, preventing request smuggling.
            if matches!(
                name.as_str(),
                "host"
                    | "content-length"
                    | "transfer-encoding"
                    | "connection"
                    | "proxy-authorization"
            ) {
                return Err(before(TransportFailure::InvalidRequest));
            }
            let mut native = reqwest::header::HeaderValue::from_str(value.as_str())
                .map_err(|_| before(TransportFailure::InvalidRequest))?;
            if matches!(value, HeaderValue::Secret(_))
                || matches!(name.as_str(), "authorization" | "x-api-key" | "api-key")
            {
                native.set_sensitive(true);
            }
            headers.append(name, native);
        }
        headers.insert(
            reqwest::header::CONNECTION,
            reqwest::header::HeaderValue::from_static("close"),
        );
        let mut builder =
            self.client
                .request(method, url)
                .headers(headers)
                .timeout(Duration::from_millis(
                    request.timeout_ms.min(limits.deadline_ms),
                ));
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }
        builder
            .build()
            .map_err(|_| before(TransportFailure::InvalidRequest))
    }
}

struct Control {
    closed: AtomicBool,
    wake: Notify,
    done: Mutex<bool>,
    terminal: Mutex<Option<TransportError>>,
    quiescent: Condvar,
}
impl Control {
    fn new() -> Self {
        Self {
            closed: AtomicBool::new(false),
            wake: Notify::new(),
            done: Mutex::new(false),
            terminal: Mutex::new(None),
            quiescent: Condvar::new(),
        }
    }
    async fn cancelled(&self, cancellation: &Cancellation) {
        loop {
            let notified = self.wake.notified();
            if self.closed.load(Ordering::SeqCst) || cancellation.is_cancelled() {
                return;
            }
            tokio::select! { _ = notified => {}, _ = tokio::time::sleep(CANCEL_POLL) => {} }
        }
    }
}
impl StreamCloser for Control {
    fn close(&self) -> Result<(), TransportError> {
        self.closed.store(true, Ordering::SeqCst);
        self.wake.notify_waiters();
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        while !*done {
            done = self.quiescent.wait(done).unwrap_or_else(|e| e.into_inner());
        }
        Ok(())
    }
}
struct Finish(Arc<Control>);
impl Drop for Finish {
    fn drop(&mut self) {
        *self.0.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.0.quiescent.notify_all();
    }
}
struct Read {
    size: usize,
    cancellation: Cancellation,
    reply: mpsc::Sender<Result<Vec<u8>, TransportError>>,
}
struct NativeStream {
    head: ResponseHead,
    control: Arc<Control>,
    requests: async_mpsc::Sender<Read>,
    terminal: Option<Result<(), TransportError>>,
    _runtime: Arc<IoRuntime>,
}
impl HttpStream for NativeStream {
    fn closer(&self) -> Arc<dyn StreamCloser> {
        self.control.clone()
    }
    fn head(&self) -> &ResponseHead {
        &self.head
    }
    fn read(
        &mut self,
        buffer: &mut [u8],
        cancellation: &Cancellation,
    ) -> Result<usize, TransportError> {
        if buffer.is_empty() {
            return Err(after(TransportFailure::InvalidRequest));
        }
        if let Some(terminal) = &self.terminal {
            return terminal.clone().map(|()| 0);
        }
        if self.control.closed.load(Ordering::SeqCst) || cancellation.is_cancelled() {
            self.control.close()?;
            return Err(after(TransportFailure::Cancelled));
        }
        let (reply, received) = mpsc::channel();
        if self
            .requests
            .try_send(Read {
                size: buffer.len().min(READ_SIZE),
                cancellation: cancellation.clone(),
                reply,
            })
            .is_err()
        {
            self.control.close()?;
            return Err(self
                .control
                .terminal
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| after(TransportFailure::Cancelled)));
        }
        let result = received.recv().unwrap_or_else(|_| {
            let _ = self.control.close();
            Err(self
                .control
                .terminal
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .unwrap_or_else(|| after(TransportFailure::Cancelled)))
        });
        match result {
            Ok(bytes) => {
                if bytes.is_empty() {
                    self.terminal = Some(Ok(()));
                }
                let size = bytes.len();
                buffer[..size].copy_from_slice(&bytes);
                Ok(size)
            }
            Err(error) => {
                self.terminal = Some(Err(error.clone()));
                self.control.close()?;
                Err(error)
            }
        }
    }
    fn close(&mut self) -> Result<(), TransportError> {
        self.control.close()
    }
}
impl Drop for NativeStream {
    fn drop(&mut self) {
        let _ = self.control.close();
    }
}

impl StreamingHttp for NativeStreamingHttp {
    fn open(
        &self,
        request: &HttpRequest,
        limits: &StreamLimits,
        cancellation: &Cancellation,
    ) -> Result<Box<dyn HttpStream>, TransportError> {
        if cancellation.is_cancelled() {
            return Err(before(TransportFailure::Cancelled));
        }
        let prepared = self.prepare(request, limits)?;
        let limits = limits.clone();
        let response_limit = limits
            .max_response_bytes
            .min(request.max_response_bytes as u64);
        let deadline = Instant::now()
            .checked_add(Duration::from_millis(
                limits.deadline_ms.min(request.timeout_ms),
            ))
            .ok_or_else(|| before(TransportFailure::InvalidRequest))?;
        let control = Arc::new(Control::new());
        let (head, received) = mpsc::channel();
        let (requests, reads) = async_mpsc::channel(1);
        let client = self.client.clone();
        let cancellation = cancellation.clone();
        let task_control = control.clone();
        self.runtime.handle.spawn(async move {
            let _finish = Finish(task_control.clone());
            let result = Transfer {
                client,
                request: prepared,
                limits,
                response_limit,
                deadline,
                control: task_control.clone(),
                cancellation,
                head: head.clone(),
                reads,
            }
            .run()
            .await;
            if let Err(error) = result {
                *task_control
                    .terminal
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
                let _ = head.send(Err(error));
            }
        });
        match received.recv() {
            Ok(Ok(head)) => Ok(Box::new(NativeStream {
                head,
                control,
                requests,
                terminal: None,
                _runtime: self.runtime.clone(),
            })),
            Ok(Err(error)) => {
                control.close()?;
                Err(error)
            }
            Err(_) => {
                control.close()?;
                Err(before(TransportFailure::Protocol))
            }
        }
    }
}

struct Transfer {
    client: reqwest::Client,
    request: reqwest::Request,
    limits: StreamLimits,
    response_limit: u64,
    deadline: Instant,
    control: Arc<Control>,
    cancellation: Cancellation,
    head: mpsc::Sender<Result<ResponseHead, TransportError>>,
    reads: async_mpsc::Receiver<Read>,
}
impl Transfer {
    async fn run(self) -> Result<(), TransportError> {
        let Self {
            client,
            request,
            limits,
            response_limit,
            deadline,
            control,
            cancellation,
            head,
            mut reads,
        } = self;
        let remaining = || tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        let mut response = tokio::select! {
            biased;
            _ = control.cancelled(&cancellation) => return Err(after(TransportFailure::Cancelled)),
            _ = remaining() => return Err(after(TransportFailure::Deadline)),
            result = tokio::time::timeout(Duration::from_millis(limits.idle_timeout_ms), client.execute(request)) => {
                match result {
                    Err(_) => return Err(after(TransportFailure::IdleTimeout)),
                    Ok(Err(error)) => return Err(classify(&error)),
                    Ok(Ok(response)) => response,
                }
            }
        };
        let mut bytes = 16usize; // status line, bounded separately by the library parser
        for (name, value) in response.headers() {
            bytes = bytes
                .checked_add(name.as_str().len() + value.as_bytes().len() + 4)
                .ok_or_else(|| after(TransportFailure::TooLarge))?;
        }
        if bytes > limits.max_header_bytes || response.headers().len() > limits.max_headers {
            return Err(after(TransportFailure::TooLarge));
        }
        let mut headers = Vec::new();
        for (name, value) in response.headers() {
            if selected(name.as_str()) {
                let value = value
                    .to_str()
                    .map_err(|_| after(TransportFailure::Protocol))?;
                headers.push((name.as_str().to_owned(), value.to_owned()));
            }
        }
        if response
            .content_length()
            .is_some_and(|n| n > response_limit)
        {
            return Err(after(TransportFailure::TooLarge));
        }
        if head
            .send(Ok(ResponseHead {
                status: response.status().as_u16(),
                headers,
            }))
            .is_err()
        {
            return Ok(());
        }
        let mut pending = None;
        let mut consumed = 0u64;
        loop {
            let read = tokio::select! {
                biased;
                _ = control.cancelled(&cancellation) => return Err(after(TransportFailure::Cancelled)),
                _ = remaining() => return Err(after(TransportFailure::Deadline)),
                read = reads.recv() => match read { Some(read) => read, None => return Ok(()) },
            };
            let result = tokio::select! {
                biased;
                _ = control.cancelled(&cancellation) => Err(after(TransportFailure::Cancelled)),
                _ = control.cancelled(&read.cancellation) => Err(after(TransportFailure::Cancelled)),
                _ = remaining() => Err(after(TransportFailure::Deadline)),
                result = tokio::time::timeout(Duration::from_millis(limits.idle_timeout_ms), async {
                    if pending.is_none() { pending = response.chunk().await.map_err(|e| classify(&e))?; }
                    match &mut pending {
                        None => Ok(Vec::new()),
                        Some(chunk) => {
                            let size = chunk.len().min(read.size);
                            consumed = consumed.checked_add(size as u64).ok_or_else(|| after(TransportFailure::TooLarge))?;
                            if consumed > response_limit { return Err(after(TransportFailure::TooLarge)); }
                            let bytes = chunk.split_to(size).to_vec();
                            if chunk.is_empty() { pending = None; }
                            Ok(bytes)
                        }
                    }
                }) => result.unwrap_or_else(|_| Err(after(TransportFailure::IdleTimeout))),
            };
            let terminal = match &result {
                Ok(bytes) => bytes.is_empty(),
                Err(_) => true,
            };
            let _ = read.reply.send(result);
            if terminal {
                return Ok(());
            }
        }
    }
}
fn classify(error: &reqwest::Error) -> TransportError {
    // Never format the error: it may contain the credential-bearing request URL.
    if error.is_connect() {
        before(TransportFailure::Unreachable)
    } else if error.is_timeout() {
        after(TransportFailure::Deadline)
    } else {
        after(TransportFailure::Protocol)
    }
}
fn selected(name: &str) -> bool {
    matches!(
        name,
        "content-type"
            | "content-length"
            | "retry-after"
            | "location"
            | "request-id"
            | "x-request-id"
            | "openai-request-id"
            | "anthropic-request-id"
            | "x-ratelimit-remaining-requests"
            | "x-ratelimit-remaining-tokens"
            | "x-ratelimit-reset-requests"
            | "x-ratelimit-reset-tokens"
    )
}
