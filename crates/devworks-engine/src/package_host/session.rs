//! One protocol session with one helper process over its stdin/stdout (or any byte streams):
//! framing, strict decoding, request-ID namespaces, outstanding-request matching, the bounded
//! broker queue and per-invocation progress.
//!
//! * Every frame is decoded strictly (core framing: one JSON value, no duplicate members) into
//!   the typed public DTO and validated before anything acts on it.
//! * The host's request IDs are `<session>:h:<n>`; a package request must use
//!   `<session>:p:<id>`. A response must match exactly one outstanding host request; an unknown
//!   or duplicate terminal response, a malformed or oversized frame, invalid UTF-8 or a package
//!   request outside its namespace fails the session visibly (the host then stops the process).
//! * The control reader always drains: package requests go to one bounded broker worker; when
//!   its queue is full the reader answers `Busy` at once instead of blocking.
//! * Progress is bounded per invocation and coalesces with a dropped-count marker; terminal
//!   results never drop.
//! * Frames to the helper go through a bounded outbox drained by the session's own writer
//!   thread: sending never blocks on the helper's input pipe, so a helper that stops reading
//!   cannot hold a request, a cancellation or shutdown past its own deadline. A full outbox
//!   (the helper stopped reading) fails the session; a frame the writer could not hand to the
//!   pipe within a bound is visible through [`Session::write_stalled`], and terminating the
//!   process (which never needs the writer) unblocks it.
use devworks_core::framing;
use devworks_extension_protocol::frame::{self, FrameError, MAX_FRAME_BYTES};
use devworks_extension_protocol::package::{
    ContributionEvent, HostRequest, PackageNotification, PackageRequest, RpcError, RpcErrorClass,
    RpcErrorData, RpcFailure, RpcFailureId, RpcResponse, RpcResult, RpcSuccess, RpcVersion,
};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Most progress notifications kept per invocation (older ones are dropped and counted).
pub const PROGRESS_KEPT: usize = 64;
/// Most produced events kept per invocation.
pub const EVENTS_KEPT: usize = 256;
/// Most bytes queued for a helper that has not read them yet (the session fails beyond it).
pub const OUTBOX_BYTES: usize = 4 * MAX_FRAME_BYTES;

/// Frames waiting for the writer thread.
#[derive(Default)]
struct Outbox {
    frames: VecDeque<Vec<u8>>,
    bytes: usize,
    closed: bool,
    /// When the writer started handing the current frame to the pipe (None while idle).
    writing_since: Option<Instant>,
}

/// What answered a host request.
#[derive(Clone, Debug)]
pub enum Reply {
    Result(RpcResult),
    Error(RpcError),
    /// The session failed or closed before an answer arrived.
    Lost(String),
}

/// One invocation as the session knows it: its identity, token and what the package reported.
pub struct Slot {
    pub invocation_id: String,
    pub token: String,
    /// The package and contribution it runs.
    pub package_id: String,
    pub contribution_id: String,
    /// Set first on cancel, timeout, disable or stop: later broker calls are refused.
    pub revoked: AtomicBool,
    pub state: Mutex<SlotState>,
    pub changed: Condvar,
    /// Host-side context the broker resolves calls against.
    pub context: Box<dyn std::any::Any + Send + Sync>,
}
#[derive(Default)]
pub struct SlotState {
    pub progress: VecDeque<(i64, String)>,
    pub dropped_progress: i64,
    pub events: Vec<ContributionEvent>,
    pub terminal: Option<Reply>,
    pub cancel_acknowledged: bool,
}
impl Slot {
    pub fn terminal(&self) -> Option<Reply> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .terminal
            .clone()
    }
    fn finish(&self, reply: Reply) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if s.terminal.is_none() {
            s.terminal = Some(reply);
        }
        drop(s);
        self.changed.notify_all();
    }
    /// Waits up to `timeout` for the terminal reply.
    pub fn wait(&self, timeout: Duration) -> Option<Reply> {
        let until = Instant::now() + timeout;
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(t) = &s.terminal {
                return Some(t.clone());
            }
            let now = Instant::now();
            if now >= until {
                return None;
            }
            s = self
                .changed
                .wait_timeout(s, until - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }
}

/// Resolves package requests (the host's broker). Called on the broker worker thread only.
pub trait Broker: Send + Sync {
    fn handle(&self, session: &Session, request: &PackageRequest) -> Result<RpcResult, RpcError>;
}

/// The error reply of a refused package request.
pub fn rpc_error(class: RpcErrorClass, message: &str) -> RpcError {
    let code = match class {
        RpcErrorClass::Busy => -32001,
        RpcErrorClass::PermissionDenied => -32003,
        RpcErrorClass::OutcomeUnknown => -32004,
        RpcErrorClass::ProtocolViolation => -32600,
        RpcErrorClass::Unavailable => -32020,
        RpcErrorClass::Conflict => -32009,
    };
    RpcError {
        code,
        message: if message.is_empty() {
            "error".into()
        } else {
            message.chars().take(4096).collect()
        },
        data: RpcErrorData {
            class,
            operation_id: None,
        },
    }
}

pub struct Session {
    pub id: String,
    outbox: Mutex<Outbox>,
    outbox_cv: Condvar,
    pending: Mutex<HashMap<String, mpsc::Sender<Reply>>>,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    /// Host request ID of each invocation's `host.invoke`, so its response finishes the slot.
    invokes: Mutex<HashMap<String, Arc<Slot>>>,
    next: AtomicU64,
    failure: Mutex<Option<String>>,
    closed: AtomicBool,
    closed_cv: Condvar,
    closed_mx: Mutex<()>,
    max_frame: usize,
}

impl Session {
    /// Starts the session's reader and broker worker over `input` (the helper's stdout).
    pub fn start(
        id: String,
        input: Box<dyn Read + Send>,
        output: Box<dyn Write + Send>,
        broker: Arc<dyn Broker>,
        queue_capacity: usize,
    ) -> Arc<Session> {
        let session = Arc::new(Session {
            id,
            outbox: Mutex::new(Outbox::default()),
            outbox_cv: Condvar::new(),
            pending: Mutex::new(HashMap::new()),
            slots: Mutex::new(HashMap::new()),
            invokes: Mutex::new(HashMap::new()),
            next: AtomicU64::new(0),
            failure: Mutex::new(None),
            closed: AtomicBool::new(false),
            closed_cv: Condvar::new(),
            closed_mx: Mutex::new(()),
            max_frame: MAX_FRAME_BYTES,
        });
        let (tx, rx) = mpsc::sync_channel::<(String, PackageRequest)>(queue_capacity.max(1));
        let worker = session.clone();
        std::thread::Builder::new()
            .name(format!("package-broker-{}", worker.id))
            .spawn(move || broker_loop(worker, broker, rx))
            .expect("spawn broker worker");
        let reader = session.clone();
        std::thread::Builder::new()
            .name(format!("package-reader-{}", reader.id))
            .spawn(move || reader.read_loop(input, tx))
            .expect("spawn session reader");
        let writer = session.clone();
        std::thread::Builder::new()
            .name(format!("package-writer-{}", writer.id))
            .spawn(move || writer.write_loop(output))
            .expect("spawn session writer");
        session
    }

    /// Why the session failed, if it did.
    pub fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
    /// Waits up to `timeout` for the stream to close.
    pub fn wait_closed(&self, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        let mut g = self.closed_mx.lock().unwrap_or_else(|p| p.into_inner());
        while !self.is_closed() {
            let now = Instant::now();
            if now >= until {
                return false;
            }
            g = self
                .closed_cv
                .wait_timeout(g, until - now)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        true
    }

    /// Fails the session: every outstanding request and invocation ends `Lost`.
    pub fn fail(&self, reason: &str) {
        {
            let mut f = self.failure.lock().unwrap_or_else(|p| p.into_inner());
            if f.is_none() {
                *f = Some(reason.chars().take(1024).collect());
            }
        }
        self.close(reason);
    }

    fn close(&self, reason: &str) {
        self.closed.store(true, Ordering::SeqCst);
        {
            let mut o = self.outbox.lock().unwrap_or_else(|p| p.into_inner());
            o.closed = true;
            o.frames.clear();
            o.bytes = 0;
        }
        self.outbox_cv.notify_all();
        let pending: Vec<_> = self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain()
            .collect();
        for (_, tx) in pending {
            let _ = tx.send(Reply::Lost(reason.to_owned()));
        }
        let slots: Vec<_> = self
            .slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect();
        for s in slots {
            s.revoked.store(true, Ordering::SeqCst);
            s.finish(Reply::Lost(reason.to_owned()));
        }
        let _g = self.closed_mx.lock().unwrap_or_else(|p| p.into_inner());
        self.closed_cv.notify_all();
    }

    /// Queues one frame for the writer thread; never blocks on the helper.
    fn write_value(&self, v: &Value) -> Result<(), String> {
        let bytes = serde_json::to_vec(v).map_err(|e| e.to_string())?;
        if bytes.len() > self.max_frame {
            return Err("the frame exceeds the frame limit".into());
        }
        let mut o = self.outbox.lock().unwrap_or_else(|p| p.into_inner());
        if o.closed {
            return Err(self.failure().unwrap_or_else(|| "session closed".into()));
        }
        if o.bytes + bytes.len() > OUTBOX_BYTES {
            return Err("the helper is not reading its input".into());
        }
        o.bytes += bytes.len();
        o.frames.push_back(bytes);
        drop(o);
        self.outbox_cv.notify_all();
        Ok(())
    }

    /// Whether the writer has been handing one frame to the helper for longer than `bound`
    /// (the helper stopped reading its input).
    pub fn write_stalled(&self, bound: Duration) -> bool {
        self.outbox
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .writing_since
            .is_some_and(|t| t.elapsed() > bound)
    }

    /// The writer thread: hands queued frames to the helper's input in order. Only this thread
    /// ever blocks on the pipe; closing the session drops the pipe once the current write ends
    /// (terminating the process ends a blocked write).
    fn write_loop(self: Arc<Self>, mut output: Box<dyn Write + Send>) {
        loop {
            let bytes = {
                let mut o = self.outbox.lock().unwrap_or_else(|p| p.into_inner());
                loop {
                    if o.closed {
                        return;
                    }
                    if let Some(b) = o.frames.pop_front() {
                        o.bytes -= b.len();
                        o.writing_since = Some(Instant::now());
                        break b;
                    }
                    o = self.outbox_cv.wait(o).unwrap_or_else(|p| p.into_inner());
                }
            };
            let written = frame::write_frame(&mut output, &bytes);
            self.outbox
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .writing_since = None;
            if let Err(e) = written {
                self.fail(&format!("write failed: {e}"));
                return;
            }
        }
    }

    fn next_id(&self) -> String {
        format!(
            "{}:h:{}",
            self.id,
            self.next.fetch_add(1, Ordering::SeqCst) + 1
        )
    }

    /// Sends a host request built by `build(id)` and waits up to `timeout` for its answer.
    pub fn request(&self, build: impl FnOnce(String) -> HostRequest, timeout: Duration) -> Reply {
        if self.is_closed() {
            return Reply::Lost(self.failure().unwrap_or_else(|| "session closed".into()));
        }
        let id = self.next_id();
        let req = build(id.clone());
        if req.validate().is_err() {
            return Reply::Lost("the host built an invalid request".into());
        }
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.clone(), tx);
        let sent = serde_json::to_value(&req)
            .map_err(|e| e.to_string())
            .and_then(|v| self.write_value(&v));
        if let Err(e) = sent {
            self.pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            self.fail(&format!("write failed: {e}"));
            return Reply::Lost(e);
        }
        match rx.recv_timeout(timeout) {
            Ok(r) => r,
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                Reply::Lost("no answer within the deadline".into())
            }
        }
    }

    /// Registers an invocation slot and sends its `host.invoke` (built by `build(id)`); the
    /// slot finishes when the package's terminal answer arrives.
    pub fn invoke(
        &self,
        slot: Arc<Slot>,
        build: impl FnOnce(String) -> HostRequest,
    ) -> Result<(), String> {
        if self.is_closed() {
            return Err(self.failure().unwrap_or_else(|| "session closed".into()));
        }
        let id = self.next_id();
        let req = build(id.clone());
        req.validate().map_err(str::to_owned)?;
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(slot.invocation_id.clone(), slot.clone());
        self.invokes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.clone(), slot.clone());
        let sent = serde_json::to_value(&req)
            .map_err(|e| e.to_string())
            .and_then(|v| self.write_value(&v));
        if let Err(e) = sent {
            self.fail(&format!("write failed: {e}"));
            return Err(e);
        }
        Ok(())
    }

    /// The slot of a live invocation whose token matches (never a revoked one).
    pub fn slot(&self, invocation_id: &str, token: &str) -> Option<Arc<Slot>> {
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(invocation_id)
            .filter(|s| s.token == token && !s.revoked.load(Ordering::SeqCst))
            .cloned()
    }
    /// Forgets a finished invocation.
    pub fn forget(&self, invocation_id: &str) {
        self.slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(invocation_id);
    }

    fn respond(&self, id: &str, outcome: Result<RpcResult, RpcError>) {
        let v = match outcome {
            Ok(result) => serde_json::to_value(RpcResponse::Success(RpcSuccess {
                jsonrpc: RpcVersion::V2,
                id: id.to_owned(),
                result,
            })),
            Err(error) => serde_json::to_value(RpcResponse::Failure(RpcFailure {
                jsonrpc: RpcVersion::V2,
                id: RpcFailureId::Known(id.to_owned()),
                error,
            })),
        };
        if let Ok(v) = v
            && let Err(e) = self.write_value(&v)
        {
            self.fail(&format!("write failed: {e}"));
        }
    }

    fn read_loop(
        self: Arc<Self>,
        mut input: Box<dyn Read + Send>,
        broker: SyncSender<(String, PackageRequest)>,
    ) {
        loop {
            let bytes = match frame::read_frame(&mut input, self.max_frame) {
                Ok(b) => b,
                Err(FrameError::Closed) => {
                    self.close("the helper closed its output");
                    return;
                }
                Err(e) => {
                    self.fail(&format!("framing: {e}"));
                    return;
                }
            };
            if let Err(why) = self.dispatch(&bytes, &broker) {
                self.fail(&why);
                return;
            }
            if self.is_closed() {
                return;
            }
        }
    }

    fn dispatch(
        &self,
        bytes: &[u8],
        broker: &SyncSender<(String, PackageRequest)>,
    ) -> Result<(), String> {
        let value = framing::parse(bytes).map_err(|e| format!("frame: {e}"))?;
        let Some(o) = value.as_object() else {
            return Err("a frame is not one JSON object (batches are refused)".into());
        };
        if o.contains_key("method") {
            if o.contains_key("id") {
                let req: PackageRequest =
                    serde_json::from_value(value.clone()).map_err(|e| format!("request: {e}"))?;
                let id = match &req {
                    PackageRequest::Call { id, .. }
                    | PackageRequest::State { id, .. }
                    | PackageRequest::CommitEvent { id, .. } => id.clone(),
                };
                if !id.starts_with(&format!("{}:p:", self.id)) {
                    return Err("a package request ID is outside its namespace".into());
                }
                if let Err(e) = req.validate() {
                    self.respond(&id, Err(rpc_error(RpcErrorClass::ProtocolViolation, e)));
                    return Ok(());
                }
                match broker.try_send((id.clone(), req)) {
                    Ok(()) => {}
                    Err(mpsc::TrySendError::Full(_)) => self.respond(
                        &id,
                        Err(rpc_error(
                            RpcErrorClass::Busy,
                            "the host broker queue is full; retry",
                        )),
                    ),
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        return Err("the broker stopped".into());
                    }
                }
                return Ok(());
            }
            let n: PackageNotification =
                serde_json::from_value(value.clone()).map_err(|e| format!("notification: {e}"))?;
            n.validate().map_err(str::to_owned)?;
            self.notify(n);
            return Ok(());
        }
        let resp: RpcResponse =
            serde_json::from_value(value).map_err(|e| format!("response: {e}"))?;
        resp.validate().map_err(str::to_owned)?;
        let (id, reply) = match resp {
            RpcResponse::Success(s) => (s.id, Reply::Result(s.result)),
            RpcResponse::Failure(f) => match f.id {
                RpcFailureId::Known(id) => (id, Reply::Error(f.error)),
                RpcFailureId::Unknown(_) => {
                    return Err(format!(
                        "the helper reported an uncorrelated error: {}",
                        f.error.message
                    ));
                }
            },
        };
        if let Some(slot) = self
            .invokes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id)
        {
            match &reply {
                Reply::Result(RpcResult::InvocationFinished { invocation_id, .. })
                    if invocation_id.as_str() == slot.invocation_id => {}
                Reply::Error(_) => {}
                _ => return Err("a host.invoke answer does not finish its invocation".into()),
            }
            slot.finish(reply);
            return Ok(());
        }
        match self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id)
        {
            Some(tx) => {
                let _ = tx.send(reply);
                Ok(())
            }
            None => Err(format!("an unknown or duplicate response {id:?}")),
        }
    }

    fn notify(&self, n: PackageNotification) {
        match n {
            PackageNotification::Progress { params, .. } => {
                if params.process_session_id.as_str() != self.id {
                    return;
                }
                let Some(slot) = self.slot(
                    params.invocation_id.as_str(),
                    params.invocation_token.as_str(),
                ) else {
                    return;
                };
                let seq = String::from(params.sequence).parse().unwrap_or(0);
                let mut s = slot.state.lock().unwrap_or_else(|p| p.into_inner());
                s.dropped_progress += String::from(params.dropped_count)
                    .parse::<i64>()
                    .unwrap_or(0);
                if s.progress.len() >= PROGRESS_KEPT {
                    s.progress.pop_front();
                    s.dropped_progress += 1;
                }
                s.progress.push_back((seq, params.text));
                drop(s);
                slot.changed.notify_all();
            }
            PackageNotification::Event { params, .. } => {
                if params.process_session_id.as_str() != self.id {
                    return;
                }
                let Some(slot) = self.slot(
                    params.invocation_id.as_str(),
                    params.invocation_token.as_str(),
                ) else {
                    return;
                };
                let mut s = slot.state.lock().unwrap_or_else(|p| p.into_inner());
                if s.events.len() < EVENTS_KEPT {
                    s.events.push(params.event);
                }
            }
            // No host stream grants credit yet; a credit grant is validated and ignored.
            PackageNotification::Credit { .. } => {}
        }
    }

    /// Records that a `host.cancel` was acknowledged for `invocation_id`.
    pub fn acknowledge_cancel(&self, invocation_id: &str) {
        if let Some(s) = self
            .slots
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(invocation_id)
        {
            s.state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .cancel_acknowledged = true;
        }
    }
}

fn broker_loop(
    session: Arc<Session>,
    broker: Arc<dyn Broker>,
    rx: Receiver<(String, PackageRequest)>,
) {
    while let Ok((id, req)) = rx.recv() {
        if session.is_closed() {
            return;
        }
        let outcome = broker.handle(&session, &req);
        session.respond(&id, outcome);
    }
}
