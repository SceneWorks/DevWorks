//! A Chrome DevTools Protocol client over [`CdpTransport`]: request/response correlation by
//! message id, flattened target sessions (`sessionId`), bounded event buffering and strict
//! base64 decoding of binary results. It owns no thread: messages are read while a call waits
//! and when the owner pumps, so every read is bounded by the caller's timeout.
use crate::ports::browser::{BrowserFault, CdpTransport};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Most buffered events; older ones are dropped (and counted) beyond it.
pub const EVENT_LIMIT: usize = 10_000;
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

pub struct Cdp {
    transport: Box<dyn CdpTransport>,
    next_id: u64,
    events: VecDeque<CdpEvent>,
    /// Events dropped because the buffer was full (reported as truncation).
    pub dropped: u64,
    closed: bool,
}

impl Cdp {
    pub fn new(transport: Box<dyn CdpTransport>) -> Self {
        Self {
            transport,
            next_id: 0,
            events: VecDeque::new(),
            dropped: 0,
            closed: false,
        }
    }

    /// Whether the transport reported closure.
    pub fn closed(&self) -> bool {
        self.closed
    }

    fn push(&mut self, event: CdpEvent) {
        if self.events.len() >= EVENT_LIMIT {
            self.events.pop_front();
            self.dropped += 1;
        }
        self.events.push_back(event);
    }

    fn read(&mut self, timeout: Duration) -> Result<Option<Value>, CdpError> {
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
                serde_json::from_str::<Value>(&text)
                    .map(Some)
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
    /// the page, so input that opened it would otherwise never be acknowledged.
    fn file(&mut self, message: Value) {
        if let Some(method) = message.get("method").and_then(Value::as_str) {
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
            self.push(event);
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
            let Some(m) = self.read(left)? else {
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
            self.file(m);
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
                Some(m) => self.file(m),
                None => return Ok(()),
            }
        }
    }

    /// The buffered events, oldest first.
    pub fn take_events(&mut self) -> Vec<CdpEvent> {
        self.events.drain(..).collect()
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
            c.push(CdpEvent {
                session: None,
                method: format!("E.{i}"),
                params: Value::Null,
            });
        }
        assert_eq!(c.dropped, 5);
        let events = c.take_events();
        assert_eq!(events.len(), EVENT_LIMIT);
        assert_eq!(events[0].method, "E.5");
    }
}
