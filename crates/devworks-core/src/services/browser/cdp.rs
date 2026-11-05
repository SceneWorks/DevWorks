//! A Chrome DevTools Protocol client over [`CdpTransport`]: request/response correlation by
//! message id, flattened target sessions (`sessionId`), bounded event buffering and strict
//! base64 decoding of binary results. It owns no thread: messages are read while a call waits
//! and when the owner pumps, so every read is bounded by the caller's timeout.
use crate::ports::browser::{BrowserFault, CdpTransport};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Most buffered events. Beyond it the oldest evidence-only event is dropped (and counted); a
/// lifecycle event (see [`lifecycle`]) is dropped only when nothing else is buffered, and that
/// loss is reported through [`Cdp::take_lifecycle_lost`].
pub const EVENT_LIMIT: usize = 10_000;
/// Aggregate wire bytes retained as parsed events (in addition to the transport budget).
pub const EVENT_BYTE_LIMIT: usize = 8 * 1024 * 1024;

/// Whether an event carries target, frame or document lifecycle: attachment, detachment,
/// creation, destruction and crashes of targets, frame attachment, detachment and navigation,
/// document replacement and load. The live state advances document revisions (which fence
/// every reference) from these, so losing one is never only a gap in the evidence.
pub fn lifecycle(method: &str) -> bool {
    method.starts_with("Target.")
        || method.starts_with("Inspector.")
        || method.starts_with("Page.frame")
        || matches!(
            method,
            "Page.navigatedWithinDocument" | "Page.lifecycleEvent" | "DOM.documentUpdated"
        )
}
/// Largest message this client accepts (a full-page screenshot can be large).
pub const MESSAGE_LIMIT: usize = 64 * 1024 * 1024;

/// One protocol notification.
#[derive(Clone, Debug, PartialEq)]
pub struct CdpEvent {
    /// The flattened target session it belongs to (`None`: the browser target).
    pub session: Option<String>,
    pub method: String,
    pub params: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CdpError {
    /// The transport failed or closed: the browser exited or the connection dropped.
    Transport(BrowserFault),
    /// No response arrived within the bound. The request was sent: its effect is unknown.
    Timeout(String),
    /// The browser answered with a protocol error.
    Remote { code: i64, message: String },
    /// A message could not be interpreted.
    Protocol(String),
}
impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "{e}"),
            Self::Timeout(m) => write!(f, "no response to {m} within its bound"),
            Self::Remote { code, message } => write!(f, "browser error {code}: {message}"),
            Self::Protocol(e) => write!(f, "protocol error: {e}"),
        }
    }
}

/// The origin artifact pages are served from: a reserved name (`.invalid` never resolves),
/// answered only by request interception inside the session's browser. Nothing listens on a
/// socket, so no other process can reach it.
pub const ARTIFACT_ORIGIN: &str = "https://artifact.devworks.invalid";

/// URL patterns intercepted on every page and frame session: the artifact origin (fulfilled
/// from the session's staged pages) and every scheme that reaches local data (always refused).
pub const INTERCEPTED: [&str; 8] = [
    "https://artifact.devworks.invalid/*",
    "file:*",
    "filesystem:*",
    "chrome:*",
    "chrome-extension:*",
    "chrome-untrusted:*",
    "devtools:*",
    "view-source:*",
];

/// Most artifact pages one session keeps ready to serve (the newest ones).
const SERVED_PAGES: usize = 8;

/// A staged artifact page served at its exact URL under [`ARTIFACT_ORIGIN`].
#[derive(Clone, Debug)]
pub struct ServedPage {
    pub url: String,
    pub body: Vec<u8>,
    pub media_type: String,
}

pub struct Cdp {
    transport: Box<dyn CdpTransport>,
    next_id: u64,
    events: VecDeque<(CdpEvent, usize)>,
    event_bytes: usize,
    /// Events dropped because the buffer was full (reported as truncation).
    pub dropped: u64,
    /// A lifecycle event was dropped since [`Self::take_lifecycle_lost`] last reported it.
    lifecycle_lost: bool,
    closed: bool,
    served: VecDeque<ServedPage>,
}

impl Cdp {
    pub fn new(transport: Box<dyn CdpTransport>) -> Self {
        Self {
            transport,
            next_id: 0,
            events: VecDeque::new(),
            event_bytes: 0,
            dropped: 0,
            lifecycle_lost: false,
            closed: false,
            served: VecDeque::new(),
        }
    }

    /// Makes `page` available at its URL (the oldest is forgotten past [`SERVED_PAGES`]).
    pub fn serve(&mut self, page: ServedPage) {
        if self.served.len() >= SERVED_PAGES {
            self.served.pop_front();
        }
        self.served.push_back(page);
    }

    /// The served page an intercepted request names exactly, if any.
    pub fn served_page(&self, url: &str) -> Option<&ServedPage> {
        let url = url.split('#').next().unwrap_or(url);
        self.served.iter().rev().find(|p| p.url == url)
    }

    /// Whether the transport reported closure.
    pub fn closed(&self) -> bool {
        self.closed
    }

    /// Buffers `event`. A full buffer gives up its oldest evidence-only event; only when every
    /// buffered event carries lifecycle is the oldest of those given up, and the loss recorded.
    fn push(&mut self, event: CdpEvent, bytes: usize) {
        if bytes > EVENT_BYTE_LIMIT {
            self.dropped += 1;
            self.lifecycle_lost |= lifecycle(&event.method);
            return;
        }
        while self.events.len() >= EVENT_LIMIT
            || self.event_bytes.saturating_add(bytes) > EVENT_BYTE_LIMIT
        {
            let removed = match self.events.iter().position(|(e, _)| !lifecycle(&e.method)) {
                Some(k) => self.events.remove(k),
                None => {
                    self.lifecycle_lost = true;
                    self.events.pop_front()
                }
            }
            .expect("a full event buffer is nonempty");
            self.event_bytes -= removed.1;
            self.dropped += 1;
        }
        self.events.push_back((event, bytes));
        self.event_bytes += bytes;
    }

    /// Whether a lifecycle event was lost since the last call: the live state then no longer
    /// follows the browser, so every reference taken before must be refused.
    pub fn take_lifecycle_lost(&mut self) -> bool {
        std::mem::take(&mut self.lifecycle_lost)
    }

    fn read(&mut self, timeout: Duration) -> Result<Option<(Value, usize)>, CdpError> {
        if self.closed {
            return Err(CdpError::Transport(BrowserFault::Closed(
                "the connection is closed".into(),
            )));
        }
        match self.transport.recv(timeout) {
            Ok(None) => Ok(None),
            Ok(Some(text)) => {
                if text.len() > MESSAGE_LIMIT {
                    return Err(CdpError::Protocol("message exceeds the size bound".into()));
                }
                let bytes = text.len();
                serde_json::from_str::<Value>(&text)
                    .map(|value| Some((value, bytes)))
                    .map_err(|e| CdpError::Protocol(format!("unreadable message: {e}")))
            }
            Err(e) => {
                self.closed = true;
                Err(CdpError::Transport(e))
            }
        }
    }

    /// Files a message that is not the awaited response: events are buffered; responses to
    /// earlier (timed-out) requests are discarded. A JavaScript dialog is answered at once
    /// (alert and beforeunload accepted, confirm and prompt dismissed): an open dialog blocks
    /// the page, so input that opened it would otherwise never be acknowledged. An intercepted
    /// request is answered at once too (the navigation awaiting it would otherwise never end):
    /// a served artifact page is fulfilled from its staged bytes; anything else intercepted (a
    /// local scheme, an unknown artifact URL) is refused as blocked by the client.
    fn file(&mut self, message: Value, bytes: usize) {
        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if method == "Fetch.requestPaused"
                && let Some(request) = message.pointer("/params/requestId").and_then(Value::as_str)
            {
                let url = message
                    .pointer("/params/request/url")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let (answer, params) = match self.served_page(url) {
                    Some(page) => (
                        "Fetch.fulfillRequest",
                        json!({
                            "requestId": request, "responseCode": 200,
                            "responseHeaders": [
                                {"name": "Content-Type", "value": page.media_type},
                                {"name": "X-Content-Type-Options", "value": "nosniff"},
                                {"name": "Cache-Control", "value": "no-store"},
                                {"name": "Referrer-Policy", "value": "no-referrer"},
                            ],
                            "body": encode_base64(&page.body),
                        }),
                    ),
                    None => (
                        "Fetch.failRequest",
                        json!({"requestId": request, "errorReason": "BlockedByClient"}),
                    ),
                };
                self.next_id += 1;
                let mut reply = json!({"id": self.next_id, "method": answer, "params": params});
                if let Some(s) = message.get("sessionId") {
                    reply["sessionId"] = s.clone();
                }
                if self.transport.send(&reply.to_string()).is_err() {
                    self.closed = true;
                }
            }
            if method == "Page.javascriptDialogOpening"
                && let Some(session) = message.get("sessionId").and_then(Value::as_str)
            {
                let kind = message
                    .pointer("/params/type")
                    .and_then(Value::as_str)
                    .unwrap_or("alert");
                self.next_id += 1;
                let answer = json!({
                    "id": self.next_id, "sessionId": session,
                    "method": "Page.handleJavaScriptDialog",
                    "params": {"accept": dialog_accepted(kind)},
                });
                if self.transport.send(&answer.to_string()).is_err() {
                    self.closed = true;
                }
            }
            let event = CdpEvent {
                session: message
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                method: method.to_owned(),
                params: message.get("params").cloned().unwrap_or(Value::Null),
            };
            self.push(event, bytes);
        }
    }

    /// Sends `method` (to the target session `session`, or the browser) and waits at most
    /// `timeout` for its response, buffering events that arrive meanwhile.
    pub fn call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CdpError> {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            message["sessionId"] = json!(s);
        }
        if self.closed {
            return Err(CdpError::Transport(BrowserFault::Closed(
                "the connection is closed".into(),
            )));
        }
        if let Err(e) = self.transport.send(&message.to_string()) {
            self.closed = true;
            return Err(CdpError::Transport(e));
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(CdpError::Timeout(method.to_owned()));
            }
            let Some((m, bytes)) = self.read(left)? else {
                continue;
            };
            if m.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = m.get("error") {
                    return Err(CdpError::Remote {
                        code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                        message: err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .chars()
                            .take(512)
                            .collect(),
                    });
                }
                return Ok(m.get("result").cloned().unwrap_or(Value::Null));
            }
            self.file(m, bytes);
        }
    }

    /// Reads whatever arrives within `budget` (events only), returning early once nothing more
    /// arrives within `quiet`.
    pub fn pump(&mut self, budget: Duration, quiet: Duration) -> Result<(), CdpError> {
        let deadline = Instant::now() + budget;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            match self.read(left.min(quiet))? {
                Some((m, bytes)) => self.file(m, bytes),
                None => return Ok(()),
            }
        }
    }

    /// The buffered events, oldest first.
    pub fn take_events(&mut self) -> Vec<CdpEvent> {
        self.event_bytes = 0;
        self.events.drain(..).map(|(event, _)| event).collect()
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.transport.close();
    }
}

/// Whether a JavaScript dialog of `kind` is accepted (alert, beforeunload) or dismissed.
pub fn dialog_accepted(kind: &str) -> bool {
    matches!(kind, "alert" | "beforeunload")
}

/// Strict standard base64 (RFC 4648 with padding), as CDP encodes binary results.
pub fn decode_base64(text: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, chunk) in bytes.chunks(4).enumerate() {
        let last = i + 1 == bytes.len() / 4;
        let pad = chunk.iter().rev().take_while(|b| **b == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for (j, b) in chunk.iter().enumerate() {
            let v = if j >= 4 - pad { 0 } else { value(*b)? };
            n = (n << 6) | v;
        }
        let triple = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        // Non-canonical padding bits are refused.
        if (pad == 1 && triple[2] != 0) || (pad == 2 && triple[1] != 0) {
            return None;
        }
        out.extend_from_slice(&triple[..3 - pad]);
    }
    Some(out)
}

/// Standard base64 with padding (test fixtures and fake endpoints).
pub fn encode_base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn base64_round_trips_and_refuses_malformed_input() {
        for case in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foob",
            b"fooba",
            b"foobar",
            &[0, 255, 7],
        ] {
            let e = encode_base64(case);
            assert_eq!(decode_base64(&e).as_deref(), Some(case), "{e}");
        }
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(decode_base64("Zm9vYg=="), Some(b"foob".to_vec()));
        for bad in ["Zm9", "Zm9v!", "Zg=a", "Z===", "Zh==", "Zm9=Zm9v", "Zm9v\n"] {
            assert_eq!(decode_base64(bad), None, "{bad}");
        }
    }

    /// A scripted transport: answers each request through `answer`, emitting events first.
    struct Scripted {
        inbox: Arc<Mutex<VecDeque<String>>>,
        answer: fn(&Value) -> Vec<Value>,
        sent: Arc<Mutex<Vec<Value>>>,
    }
    impl CdpTransport for Scripted {
        fn send(&mut self, message: &str) -> Result<(), BrowserFault> {
            let v: Value = serde_json::from_str(message).unwrap();
            self.sent.lock().unwrap().push(v.clone());
            let mut inbox = self.inbox.lock().unwrap();
            for m in (self.answer)(&v) {
                inbox.push_back(m.to_string());
            }
            Ok(())
        }
        fn recv(&mut self, _: Duration) -> Result<Option<String>, BrowserFault> {
            match self.inbox.lock().unwrap().pop_front() {
                Some(m) if m == "CLOSE" => Err(BrowserFault::Closed("peer closed".into())),
                other => Ok(other),
            }
        }
        fn close(&mut self) {}
    }

    fn cdp(answer: fn(&Value) -> Vec<Value>) -> (Cdp, Arc<Mutex<Vec<Value>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let t = Scripted {
            inbox: Arc::new(Mutex::new(VecDeque::new())),
            answer,
            sent: sent.clone(),
        };
        (Cdp::new(Box::new(t)), sent)
    }

    #[test]
    fn a_call_matches_its_response_and_buffers_interleaved_events() {
        let (mut c, sent) = cdp(|req| {
            let id = req["id"].as_u64().unwrap();
            vec![
                json!({"id": id + 100, "result": {"stale": true}}),
                json!({"method": "Runtime.consoleAPICalled", "sessionId": "s1", "params": {"type": "log"}}),
                json!({"id": id, "result": {"ok": id}}),
            ]
        });
        let r = c
            .call(
                Some("s1"),
                "Runtime.enable",
                json!({}),
                Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(r, json!({"ok": 1}));
        assert_eq!(sent.lock().unwrap()[0]["sessionId"], "s1");
        let events = c.take_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session.as_deref(), Some("s1"));
        assert_eq!(events[0].method, "Runtime.consoleAPICalled");
    }

    #[test]
    fn remote_errors_timeouts_and_closure_are_distinct() {
        let (mut c, _) = cdp(|req| {
            vec![json!({"id": req["id"], "error": {"code": -32000, "message": "No node"}})]
        });
        assert_eq!(
            c.call(None, "DOM.focus", json!({}), Duration::from_secs(1)),
            Err(CdpError::Remote {
                code: -32000,
                message: "No node".into()
            })
        );
        let (mut c, _) = cdp(|_| vec![]);
        assert_eq!(
            c.call(
                None,
                "Input.insertText",
                json!({}),
                Duration::from_millis(20)
            ),
            Err(CdpError::Timeout("Input.insertText".into()))
        );
        let (mut c, _) = cdp(|_| vec![Value::String("CLOSE".into())]);
        // The scripted inbox stores the raw text; "CLOSE" (a JSON string) is not the sentinel.
        assert!(
            c.call(
                None,
                "Browser.getVersion",
                json!({}),
                Duration::from_millis(20)
            )
            .is_err()
        );
    }

    #[test]
    fn the_event_buffer_is_bounded_and_counts_drops() {
        let (mut c, _) = cdp(|_| vec![]);
        for i in 0..(EVENT_LIMIT + 5) {
            c.push(
                CdpEvent {
                    session: None,
                    method: format!("E.{i}"),
                    params: Value::Null,
                },
                1,
            );
        }
        assert_eq!(c.dropped, 5);
        let events = c.take_events();
        assert_eq!(events.len(), EVENT_LIMIT);
        assert_eq!(events[0].method, "E.5");
        assert!(!c.take_lifecycle_lost(), "only evidence was dropped");
    }

    #[test]
    fn a_full_buffer_gives_up_evidence_before_lifecycle_and_reports_lifecycle_loss() {
        let event = |method: &str| CdpEvent {
            session: Some("S1".into()),
            method: method.into(),
            params: Value::Null,
        };
        let (mut c, _) = cdp(|_| vec![]);
        c.push(event("Page.frameNavigated"), 1);
        for _ in 0..EVENT_LIMIT {
            c.push(event("Runtime.consoleAPICalled"), 1);
        }
        c.push(event("Target.detachedFromTarget"), 1);
        let events = c.take_events();
        assert_eq!(events.len(), EVENT_LIMIT);
        assert_eq!(c.dropped, 2);
        assert_eq!(events[0].method, "Page.frameNavigated");
        assert_eq!(events[EVENT_LIMIT - 1].method, "Target.detachedFromTarget");
        assert!(!c.take_lifecycle_lost());
        // Nothing evidence-only left to give up: the oldest lifecycle event goes, and the loss
        // is reported once.
        for _ in 0..=EVENT_LIMIT {
            c.push(event("DOM.documentUpdated"), 1);
        }
        assert!(c.take_lifecycle_lost());
        assert!(!c.take_lifecycle_lost());
        for m in [
            "Target.attachedToTarget",
            "Target.targetCrashed",
            "Inspector.targetCrashed",
            "Page.frameAttached",
            "Page.frameDetached",
            "Page.frameNavigated",
            "Page.navigatedWithinDocument",
            "Page.lifecycleEvent",
            "DOM.documentUpdated",
        ] {
            assert!(lifecycle(m), "{m}");
        }
        for m in [
            "Runtime.consoleAPICalled",
            "Runtime.exceptionThrown",
            "Log.entryAdded",
            "Network.requestWillBeSent",
            "Network.responseReceived",
            "Network.loadingFailed",
        ] {
            assert!(!lifecycle(m), "{m}");
        }
    }
    #[test]
    fn large_events_are_bounded_by_bytes_and_lifecycle_loss_invalidates_references() {
        let (mut c, _) = cdp(|request| {
            let text = "x".repeat(EVENT_BYTE_LIMIT / 4);
            let mut events = (0..6)
                .map(|_| json!({"method":"Runtime.consoleAPICalled","params":{"text":text}}))
                .collect::<Vec<_>>();
            events.push(json!({"id":request["id"],"result":{}}));
            events
        });
        c.call(
            None,
            "Browser.getVersion",
            json!({}),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(
            c.event_bytes <= EVENT_BYTE_LIMIT,
            "buffered events must fit their byte budget"
        );
        assert!(c.dropped >= 3);
        assert!(!c.take_lifecycle_lost());
        c.push(
            CdpEvent {
                session: None,
                method: "Page.frameNavigated".into(),
                params: Value::Null,
            },
            EVENT_BYTE_LIMIT + 1,
        );
        assert!(
            c.take_lifecycle_lost(),
            "an oversized lifecycle event must fence old references"
        );
        c.take_events();
        assert_eq!(c.event_bytes, 0);
    }
}
