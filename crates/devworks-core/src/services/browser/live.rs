//! One running owned browser: its CDP connection, registered targets with their frame and
//! document revisions, accessibility snapshots and node maps, and the bounded, redacted
//! notification buffers. Everything here is ephemeral and dies with the browser process.
//!
//! **Revisions.** Every main-frame navigation, document replacement, frame attach/detach/
//! navigation and renderer crash advances the target's document revision, which invalidates
//! every node reference into it. A snapshot is the only source of node references: a node ID
//! names the exact DOM node (its backend node ID) the snapshot saw, and is honoured only while
//! the target's document revision still equals the snapshot's. A stale reference is refused;
//! nothing is ever re-resolved by role, name or selector.
//!
//! **Untrusted output.** Page text, accessibility names, console messages and URLs are bounded
//! and passed through the host redactor before they become evidence; URLs lose user info,
//! fragments and the values of credential-shaped query parameters. No header, cookie or body is
//! captured, and no network interception or page script is used.
use super::cdp::{Cdp, CdpError, CdpEvent, decode_base64, dialog_accepted};
use crate::authority::redact::Redactor;
use crate::ports::browser::{MAX_TARGETS, Profile};
use crate::protocol::records::{
    BrowserInput, BrowserNode, BrowserTarget, KeyModifier, MouseButton,
};
use crate::protocol::{Id, Revision};
use crate::services::process::OwnedProcess;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Most notifications buffered per channel before publication; older ones are dropped.
pub const NOTICE_LIMIT: usize = 2_000;
/// Longest text kept from one notification, name or value.
pub const TEXT_LIMIT: usize = 2_048;
/// Most accessibility nodes one snapshot records.
pub const NODE_LIMIT: usize = 5_000;
/// Most frames (and out-of-process frames) one target registers; a page with more keeps the
/// first ones addressable and its snapshots say the list was cut.
pub const MAX_FRAMES: usize = 256;
/// Snapshots retained per target (older node references stop resolving).
pub const SNAPSHOTS_KEPT: usize = 8;
/// Largest evidence document one observation publishes.
pub const EVIDENCE_LIMIT: usize = 256 * 1024;
/// Largest screenshot accepted.
pub const SCREENSHOT_LIMIT: usize = 32 * 1024 * 1024;
/// Largest full-page capture edge (CSS pixels); taller pages are clipped.
pub const CAPTURE_EDGE: f64 = 16_384.0;

/// Permissions denied for every page of an owned profile (W3C permission names).
pub const DENIED_PERMISSIONS: [&str; 12] = [
    "geolocation",
    "notifications",
    "midi",
    "camera",
    "microphone",
    "clipboard-read",
    "clipboard-write",
    "background-sync",
    "persistent-storage",
    "idle-detection",
    "display-capture",
    "window-management",
];

/// Durations the live session works within.
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub call: Duration,
    pub navigation: Duration,
    /// How long an input waits for its effects (navigation, dialogs, new targets) to show.
    pub settle: Duration,
}

/// A node an accessibility snapshot recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRef {
    pub backend: i64,
    pub role: String,
    /// The CDP session whose document holds the node (an out-of-process frame's own session).
    pub session: String,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub revision: i64,
    pub frame: String,
    pub document_revision: i64,
    pub nodes: HashMap<String, NodeRef>,
}

/// One frame of a target: its host ID, its CDP frame ID and the CDP session hosting its
/// document (the target's own session, or the session of the out-of-process frame it is in).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub host: String,
    pub cdp: String,
    pub session: String,
}

/// An out-of-process frame attached under a target (`Target.setAutoAttach`, flattened).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Child {
    /// The child's own CDP session.
    pub session: String,
    /// Its frame (the iframe target's ID equals its frame ID).
    pub frame: String,
    /// The session of the document that embeds it.
    pub parent: String,
}

#[derive(Clone, Debug)]
pub struct Target {
    /// Host target ID (`t1`, `t2`, ...).
    pub id: String,
    pub cdp_target: String,
    pub cdp_session: String,
    /// `initial`, `opened` (by `browser.navigate` without a target) or `popup`.
    pub origin: &'static str,
    pub opener: Option<String>,
    /// The target's frames; the first is the main frame (`<target>-f0`).
    pub frames: Vec<Frame>,
    /// Out-of-process frames attached under the target.
    pub children: Vec<Child>,
    /// More frames attached than [`MAX_FRAMES`] (the rest are not addressable).
    pub frames_cut: bool,
    next_frame: u64,
    pub document_revision: i64,
    pub snapshots: VecDeque<Snapshot>,
    next_snapshot: i64,
    pub url: String,
    pub crashed: bool,
    pub closed: bool,
    /// Loader IDs whose `load` lifecycle event was seen.
    loaded: Vec<String>,
}
impl Target {
    fn new(id: String, cdp_target: String, cdp_session: String, origin: &'static str) -> Self {
        let main = format!("{id}-f0");
        Self {
            frames: vec![Frame {
                host: main,
                cdp: cdp_target.clone(),
                session: cdp_session.clone(),
            }],
            children: Vec::new(),
            frames_cut: false,
            id,
            cdp_target,
            cdp_session,
            origin,
            opener: None,
            next_frame: 1,
            document_revision: 1,
            snapshots: VecDeque::new(),
            next_snapshot: 0,
            url: "about:blank".into(),
            crashed: false,
            closed: false,
            loaded: Vec::new(),
        }
    }
    pub fn main_frame(&self) -> &str {
        &self.frames[0].host
    }
    fn invalidate(&mut self) {
        self.document_revision += 1;
        self.snapshots.clear();
    }
    fn frame_of_cdp(&self, cdp: &str) -> Option<usize> {
        self.frames.iter().position(|f| f.cdp == cdp)
    }
    fn child_of_session(&self, session: &str) -> Option<&Child> {
        self.children.iter().find(|c| c.session == session)
    }
    /// Whether `session` is the target's own (top-level) session.
    fn is_page(&self, session: Option<&str>) -> bool {
        session == Some(self.cdp_session.as_str())
    }
    pub fn wire(&self, session: &Id, frame: &str) -> BrowserTarget {
        BrowserTarget {
            session_id: session.clone(),
            target_id: Id::new(self.id.clone()).expect("host target id"),
            frame_id: Id::new(frame.to_owned()).expect("host frame id"),
            document_revision: Revision::new(self.document_revision).expect("positive"),
        }
    }
}

/// A bounded buffer of redacted notifications awaiting publication.
#[derive(Debug, Default)]
pub struct Notices {
    pending: VecDeque<Value>,
    pub dropped: u64,
    pub total: u64,
}
impl Notices {
    fn push(&mut self, entry: Value) {
        if self.pending.len() >= NOTICE_LIMIT {
            self.pending.pop_front();
            self.dropped += 1;
        }
        self.total += 1;
        self.pending.push_back(entry);
    }
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty() && self.dropped == 0
    }
    /// Takes the oldest pending entries that fit one bounded evidence document (the rest stay
    /// pending for the next one) with the drop count. Until the batch is durably published
    /// it must be given back with [`Self::restore`] if publication fails.
    pub fn take(&mut self) -> NoticeBatch {
        let mut entries = Vec::new();
        let mut size = 0;
        while let Some(e) = self.pending.front() {
            let n = e.to_string().len() + 1;
            if !entries.is_empty() && size + n > EVIDENCE_LIMIT - 1024 {
                break;
            }
            size += n;
            entries.push(self.pending.pop_front().expect("front"));
        }
        NoticeBatch {
            entries,
            dropped: std::mem::take(&mut self.dropped),
        }
    }
    /// Puts back a batch whose publication failed, ahead of what arrived meanwhile (still
    /// bounded: the oldest are dropped, and counted, beyond the limit).
    pub fn restore(&mut self, batch: NoticeBatch) {
        self.dropped += batch.dropped;
        for e in batch.entries.into_iter().rev() {
            self.pending.push_front(e);
        }
        while self.pending.len() > NOTICE_LIMIT {
            self.pending.pop_front();
            self.dropped += 1;
        }
    }
}

/// Notifications taken for one observation.
#[derive(Debug)]
pub struct NoticeBatch {
    pub entries: Vec<Value>,
    pub dropped: u64,
}
impl NoticeBatch {
    /// The batch as one evidence document; `truncated` when notifications were dropped.
    pub fn document(&self, session: &Id, channel: &str, cdp_dropped: bool) -> (Vec<u8>, bool) {
        let doc = json!({
            "sessionId": session.as_str(),
            "channel": channel,
            "entries": self.entries,
            "dropped": self.dropped,
            "protocolEventsDropped": cdp_dropped,
        });
        let truncated = self.dropped > 0 || cdp_dropped;
        (doc.to_string().into_bytes(), truncated)
    }
}

/// Why an action did not complete.
#[derive(Debug)]
pub enum ActionError {
    /// A reference no longer matches the live state; nothing was sent.
    Stale { kind: &'static str, detail: String },
    /// Refused before any input was sent (unsupported key, not a file input, no box).
    Refused { kind: &'static str, detail: String },
    /// The browser failed a request before any input was sent.
    Before(CdpError),
    /// The request was sent and its effect is unknown (timeout, transport loss).
    After(CdpError),
    /// The browser reported a navigation failure (the document changed to its error page).
    Navigation(String),
    /// The session is closing.
    Closing,
}

pub struct Live {
    pub session: Id,
    pub profile: Profile,
    pub process: OwnedProcess,
    pub cdp: Cdp,
    pub product: String,
    pub protocol: String,
    pub targets: Vec<Target>,
    next_target: u64,
    pub console: Notices,
    pub network: Notices,
    /// The registered target set changed since the session record was last written.
    pub targets_changed: bool,
    /// Permissions whose denial the browser accepted / refused, and the download policy.
    pub policy: Value,
    pending_attach: Vec<Value>,
    /// Out-of-process frames attached but not configured yet: (target, child).
    pending_children: Vec<(usize, Child)>,
    started: Instant,
}

fn text(s: &str) -> String {
    let cut: String = s.chars().take(TEXT_LIMIT).collect();
    Redactor::new().redact_text(&cut)
}

/// The URL as evidence: user info, fragment and the values of credential-shaped query
/// parameters are removed; the rest is bounded and redacted.
pub fn safe_url(url: &str) -> String {
    // The fragment never leaves the page; it is dropped before anything else is read.
    let url = url.split('#').next().unwrap_or(url);
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    let base = match base.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = match rest.find('/') {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, ""),
            };
            let host = authority.rsplit('@').next().unwrap_or(authority);
            format!("{scheme}://{host}{path}")
        }
        None => base.to_owned(),
    };
    let mut out = base;
    if let Some(q) = query {
        let parts: Vec<String> = q
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| match p.split_once('=') {
                Some((k, _)) if sensitive_param(k) => format!("{k}=[REDACTED]"),
                _ => p.to_owned(),
            })
            .collect();
        if !parts.is_empty() {
            out.push('?');
            out.push_str(&parts.join("&"));
        }
    }
    text(&out)
}
fn sensitive_param(name: &str) -> bool {
    let n: String = name
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    [
        "token",
        "key",
        "secret",
        "password",
        "passwd",
        "auth",
        "code",
        "sig",
        "session",
        "credential",
        "jwt",
        "access",
        "refresh",
        "otp",
        "nonce",
        "ticket",
    ]
    .iter()
    .any(|s| n.contains(s))
}

impl Live {
    pub fn new(session: Id, profile: Profile, process: OwnedProcess, cdp: Cdp) -> Self {
        Self {
            session,
            profile,
            process,
            cdp,
            product: String::new(),
            protocol: String::new(),
            targets: Vec::new(),
            next_target: 0,
            console: Notices::default(),
            network: Notices::default(),
            targets_changed: false,
            policy: Value::Null,
            pending_attach: Vec::new(),
            pending_children: Vec::new(),
            started: Instant::now(),
        }
    }

    fn at(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CdpError> {
        let r = self.cdp.call(session, method, params, timeout);
        self.absorb_events();
        r
    }

    /// Configures the browser after connecting: version handshake, download and permission
    /// policy, target discovery and auto-attach, and every initial page target.
    /// `startup` bounds the first request: the browser answers it once it has started.
    pub fn setup(&mut self, bounds: &Bounds, startup: Duration) -> Result<(), CdpError> {
        let version = self.call(None, "Browser.getVersion", json!({}), startup)?;
        self.product = version
            .get("product")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .chars()
            .take(128)
            .collect();
        self.protocol = version
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .chars()
            .take(32)
            .collect();
        if self.protocol != "1.3" {
            return Err(CdpError::Protocol(format!(
                "unsupported DevTools protocol version {}",
                self.protocol
            )));
        }
        // Downloads are denied: no download destination grant exists for a browser session.
        self.call(
            None,
            "Browser.setDownloadBehavior",
            json!({"behavior": "deny", "eventsEnabled": true}),
            bounds.call,
        )?;
        let mut denied = Vec::new();
        let mut refused = Vec::new();
        for p in DENIED_PERMISSIONS {
            match self.call(
                None,
                "Browser.setPermission",
                json!({"permission": {"name": p}, "setting": "denied"}),
                bounds.call,
            ) {
                Ok(_) => denied.push(p),
                Err(CdpError::Remote { message, .. }) => {
                    refused.push(json!({"permission": p, "error": text(&message)}))
                }
                Err(e) => return Err(e),
            }
        }
        self.policy = json!({
            "downloads": "denied",
            "dialogs": "alert and beforeunload accepted, confirm and prompt dismissed",
            "permissionsDenied": denied,
            "permissionsNotApplied": refused,
            "networkInterception": false,
            "pageScripts": false,
        });
        self.call(
            None,
            "Target.setDiscoverTargets",
            json!({"discover": true}),
            bounds.call,
        )?;
        self.call(
            None,
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true}),
            bounds.call,
        )?;
        let targets = self.call(None, "Target.getTargets", json!({}), bounds.call)?;
        for info in targets
            .get("targetInfos")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if info.get("type").and_then(Value::as_str) == Some("page") {
                if self.open_count() >= MAX_TARGETS {
                    self.refuse_target(&info, bounds);
                    continue;
                }
                self.attach(&info, "initial", bounds)?;
            }
        }
        if self.targets.is_empty() {
            return Err(CdpError::Protocol("the browser has no page target".into()));
        }
        self.settle_pending(bounds)?;
        Ok(())
    }

    fn registered(&self, cdp_target: &str) -> bool {
        self.targets.iter().any(|t| t.cdp_target == cdp_target)
    }

    /// Attaches to a page target and registers it under the session.
    fn attach(
        &mut self,
        info: &Value,
        origin: &'static str,
        bounds: &Bounds,
    ) -> Result<Option<usize>, CdpError> {
        let Some(cdp_target) = info.get("targetId").and_then(Value::as_str) else {
            return Ok(None);
        };
        if self.registered(cdp_target) {
            return Ok(self.targets.iter().position(|t| t.cdp_target == cdp_target));
        }
        let attached = self.cdp.call(
            None,
            "Target.attachToTarget",
            json!({"targetId": cdp_target, "flatten": true}),
            bounds.call,
        )?;
        let Some(session) = attached.get("sessionId").and_then(Value::as_str) else {
            return Err(CdpError::Protocol("attach returned no session".into()));
        };
        Ok(Some(self.register(info, session, origin, bounds)?))
    }

    fn register(
        &mut self,
        info: &Value,
        cdp_session: &str,
        origin: &'static str,
        bounds: &Bounds,
    ) -> Result<usize, CdpError> {
        let cdp_target = info
            .get("targetId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if let Some(i) = self.targets.iter().position(|t| t.cdp_target == cdp_target) {
            return Ok(i);
        }
        self.next_target += 1;
        let id = format!("t{}", self.next_target);
        let mut t = Target::new(id, cdp_target, cdp_session.to_owned(), origin);
        t.url = info
            .get("url")
            .and_then(Value::as_str)
            .map(safe_url)
            .unwrap_or_else(|| "about:blank".into());
        t.opener = info
            .get("openerId")
            .and_then(Value::as_str)
            .and_then(|o| self.targets.iter().find(|x| x.cdp_target == o))
            .map(|x| x.id.clone());
        let origin = if t.opener.is_some() && origin != "opened" {
            "popup"
        } else {
            origin
        };
        t.origin = origin;
        self.targets.push(t);
        self.targets_changed = true;
        let i = self.targets.len() - 1;
        if origin == "popup" {
            let (id, opener) = (self.targets[i].id.clone(), self.targets[i].opener.clone());
            let at = self.at();
            self.console.push(json!({
                "at": at, "kind": "popup", "target": id, "opener": opener,
                "url": self.targets[i].url,
            }));
        }
        let s = cdp_session.to_owned();
        for (method, params) in [
            ("Page.enable", json!({})),
            ("Page.setLifecycleEventsEnabled", json!({"enabled": true})),
            ("Runtime.enable", json!({})),
            ("Network.enable", json!({})),
            ("Log.enable", json!({})),
            ("DOM.enable", json!({})),
            // Out-of-process (cross-site) frames attach as flattened child sessions.
            (
                "Target.setAutoAttach",
                json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true}),
            ),
        ] {
            self.cdp.call(Some(&s), method, params, bounds.call)?;
        }
        let tree = self
            .cdp
            .call(Some(&s), "Page.getFrameTree", json!({}), bounds.call)?;
        if let Some(main) = tree.pointer("/frameTree/frame/id").and_then(Value::as_str) {
            self.targets[i].frames[0].cdp = main.to_owned();
            if let Some(url) = tree.pointer("/frameTree/frame/url").and_then(Value::as_str) {
                self.targets[i].url = safe_url(url);
            }
        }
        let mut children = Vec::new();
        collect_frames(tree.pointer("/frameTree/childFrames"), &mut children);
        for c in children {
            self.add_frame(i, &c, &s);
        }
        Ok(i)
    }

    /// Records `cdp_frame` under `target`, hosted by `session` (a frame already known keeps its
    /// host ID; an out-of-process frame moves to its own session).
    fn add_frame(&mut self, target: usize, cdp_frame: &str, session: &str) {
        let t = &mut self.targets[target];
        let hosted = t
            .children
            .iter()
            .find(|c| c.frame == cdp_frame)
            .map_or(session, |c| c.session.as_str())
            .to_owned();
        match t.frame_of_cdp(cdp_frame) {
            Some(k) => t.frames[k].session = hosted,
            None if t.frames.len() >= MAX_FRAMES => t.frames_cut = true,
            None => {
                let id = format!("{}-f{}", t.id, t.next_frame);
                t.next_frame += 1;
                t.frames.push(Frame {
                    host: id,
                    cdp: cdp_frame.to_owned(),
                    session: hosted,
                });
            }
        }
    }

    fn target_of_session(&self, cdp_session: Option<&str>) -> Option<usize> {
        let s = cdp_session?;
        self.targets
            .iter()
            .position(|t| t.cdp_session == s || t.child_of_session(s).is_some())
    }

    /// Configures an attached out-of-process frame: its domains, its own auto-attach (nested
    /// frames) and its frame tree, all on its session.
    fn configure_child(&mut self, i: usize, child: Child, bounds: &Bounds) -> Result<(), CdpError> {
        let s = child.session.clone();
        for (method, params) in [
            ("Page.enable", json!({})),
            ("Page.setLifecycleEventsEnabled", json!({"enabled": true})),
            ("Runtime.enable", json!({})),
            ("Network.enable", json!({})),
            ("Log.enable", json!({})),
            ("DOM.enable", json!({})),
            (
                "Target.setAutoAttach",
                json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true}),
            ),
            ("Runtime.runIfWaitingForDebugger", json!({})),
        ] {
            self.cdp.call(Some(&s), method, params, bounds.call)?;
        }
        let tree = self
            .cdp
            .call(Some(&s), "Page.getFrameTree", json!({}), bounds.call)?;
        let mut frames = vec![child.frame.clone()];
        collect_frames(tree.pointer("/frameTree/childFrames"), &mut frames);
        for f in frames {
            self.add_frame(i, &f, &s);
        }
        self.targets[i].invalidate();
        Ok(())
    }

    /// Detaches an out-of-process frame's session: its frames leave the target.
    fn detach_child(&mut self, i: usize, session: &str) {
        let t = &mut self.targets[i];
        t.children.retain(|c| c.session != session);
        let mut k = 1;
        while k < t.frames.len() {
            if t.frames[k].session == session {
                t.frames.remove(k);
            } else {
                k += 1;
            }
        }
        t.invalidate();
    }

    /// Applies buffered events to the live state.
    pub fn absorb_events(&mut self) {
        for e in self.cdp.take_events() {
            self.apply(e);
        }
    }

    fn apply(&mut self, e: CdpEvent) {
        let at = self.at();
        let ti = self.target_of_session(e.session.as_deref());
        let tid = ti.map(|i| self.targets[i].id.clone());
        let p = &e.params;
        match e.method.as_str() {
            "Target.attachedToTarget" => {
                let info = p.get("targetInfo").cloned().unwrap_or(Value::Null);
                match info.get("type").and_then(Value::as_str) {
                    Some("page") => self.pending_attach.push(json!({
                        "info": info, "sessionId": p.get("sessionId"),
                    })),
                    Some("iframe") => {
                        if let (Some(i), Some(parent), Some(child), Some(frame)) = (
                            ti,
                            e.session.clone(),
                            p.get("sessionId").and_then(Value::as_str),
                            info.get("targetId").and_then(Value::as_str),
                        ) {
                            let c = Child {
                                session: child.to_owned(),
                                frame: frame.to_owned(),
                                parent,
                            };
                            if self.targets[i].child_of_session(child).is_none()
                                && self.targets[i].children.len() < MAX_FRAMES
                            {
                                self.targets[i].children.push(c.clone());
                                self.pending_children.push((i, c));
                            }
                        }
                    }
                    _ => {}
                }
            }
            "Target.targetCreated" => {
                let info = p.get("targetInfo").cloned().unwrap_or(Value::Null);
                if info.get("type").and_then(Value::as_str) == Some("page")
                    && info.get("attached").and_then(Value::as_bool) != Some(true)
                {
                    self.pending_attach.push(json!({"info": info}));
                }
            }
            "Target.targetDestroyed" | "Target.detachedFromTarget" => {
                let cdp = p.get("targetId").and_then(Value::as_str);
                let sess = p.get("sessionId").and_then(Value::as_str);
                // An out-of-process frame went away (removed, or navigated same-site).
                let child = self.targets.iter().enumerate().find_map(|(i, t)| {
                    t.children
                        .iter()
                        .find(|c| Some(c.session.as_str()) == sess || Some(c.frame.as_str()) == cdp)
                        .map(|c| (i, c.session.clone()))
                });
                if let Some((i, session)) = child {
                    self.detach_child(i, &session);
                } else if let Some(t) = self.targets.iter_mut().find(|t| {
                    !t.closed
                        && (Some(t.cdp_target.as_str()) == cdp
                            || Some(t.cdp_session.as_str()) == sess)
                }) {
                    t.closed = true;
                    t.invalidate();
                    self.targets_changed = true;
                    let id = t.id.clone();
                    self.console
                        .push(json!({"at": at, "kind": "targetClosed", "target": id}));
                }
            }
            "Target.targetCrashed" | "Inspector.targetCrashed"
                if ti.is_some_and(|i| !self.targets[i].is_page(e.session.as_deref())) =>
            {
                // An out-of-process frame's renderer crashed: its document is gone.
                let i = ti.expect("checked");
                self.targets[i].invalidate();
                self.console
                    .push(json!({"at": at, "kind": "frameCrash", "target": tid}));
            }
            "Target.targetCrashed" | "Inspector.targetCrashed" => {
                let cdp = p.get("targetId").and_then(Value::as_str);
                let idx = cdp
                    .and_then(|c| self.targets.iter().position(|t| t.cdp_target == c))
                    .or(ti);
                if let Some(i) = idx {
                    let t = &mut self.targets[i];
                    t.crashed = true;
                    t.invalidate();
                    self.targets_changed = true;
                    let id = t.id.clone();
                    self.console.push(json!({
                        "at": at, "kind": "crash", "target": id,
                        "status": p.get("status").and_then(Value::as_str).map(text),
                    }));
                }
            }
            "Page.frameAttached" => {
                if let (Some(i), Some(f), Some(s)) = (
                    ti,
                    p.get("frameId").and_then(Value::as_str),
                    e.session.as_deref(),
                ) {
                    self.add_frame(i, f, s);
                    self.targets[i].invalidate();
                }
            }
            "Page.frameDetached" => {
                if let (Some(i), Some(f)) = (ti, p.get("frameId").and_then(Value::as_str)) {
                    let t = &mut self.targets[i];
                    // A frame that moved to its own process stays (its child session hosts it).
                    if let Some(k) = t
                        .frame_of_cdp(f)
                        .filter(|k| *k > 0 && t.children.iter().all(|c| c.frame != f))
                    {
                        t.frames.remove(k);
                    }
                    t.invalidate();
                }
            }
            "Page.frameNavigated" => {
                if let Some(i) = ti {
                    let frame = p.get("frame").cloned().unwrap_or(Value::Null);
                    let id = frame.get("id").and_then(Value::as_str).unwrap_or_default();
                    let url = frame.get("url").and_then(Value::as_str).unwrap_or_default();
                    let page = self.targets[i].is_page(e.session.as_deref());
                    if frame.get("parentId").is_none() && page {
                        let t = &mut self.targets[i];
                        t.frames.truncate(1);
                        t.children.clear();
                        t.frames_cut = false;
                        t.frames[0].cdp = id.to_owned();
                        t.url = safe_url(url);
                    } else if let Some(s) = e.session.as_deref() {
                        self.add_frame(i, id, s);
                    }
                    self.targets[i].invalidate();
                }
            }
            "Page.navigatedWithinDocument" => {
                if let Some(i) = ti.filter(|i| self.targets[*i].is_page(e.session.as_deref())) {
                    let main = self.targets[i].frames[0].cdp.clone();
                    if p.get("frameId").and_then(Value::as_str) == Some(main.as_str()) {
                        let url = p.get("url").and_then(Value::as_str).unwrap_or_default();
                        self.targets[i].url = safe_url(url);
                    }
                }
            }
            "DOM.documentUpdated" => {
                if let Some(i) = ti {
                    self.targets[i].invalidate();
                }
            }
            "Page.lifecycleEvent" => {
                if let Some(i) = ti.filter(|i| self.targets[*i].is_page(e.session.as_deref()))
                    && p.get("name").and_then(Value::as_str) == Some("load")
                    && let Some(l) = p.get("loaderId").and_then(Value::as_str)
                {
                    let t = &mut self.targets[i];
                    if t.loaded.len() >= 64 {
                        t.loaded.remove(0);
                    }
                    t.loaded.push(l.to_owned());
                }
            }
            "Page.javascriptDialogOpening" => {
                // The client answered it as it arrived (see `Cdp`); this records the notice.
                let kind = p.get("type").and_then(Value::as_str).unwrap_or("alert");
                let accept = dialog_accepted(kind);
                self.console.push(json!({
                    "at": at, "kind": "dialog", "target": tid, "dialog": kind,
                    "message": p.get("message").and_then(Value::as_str).map(text),
                    "handled": if accept { "accepted" } else { "dismissed" },
                }));
            }
            "Browser.downloadWillBegin" | "Page.downloadWillBegin" => {
                self.console.push(json!({
                    "at": at, "kind": "download", "target": tid, "handled": "denied",
                    "url": p.get("url").and_then(Value::as_str).map(safe_url),
                    "suggestedFilename": p.get("suggestedFilename").and_then(Value::as_str).map(text),
                }));
            }
            "Runtime.consoleAPICalled" => {
                let args: Vec<String> = p
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().take(16).map(remote_text).collect())
                    .unwrap_or_default();
                self.console.push(json!({
                    "at": at, "kind": "console", "target": tid,
                    "level": p.get("type").and_then(Value::as_str).unwrap_or("log"),
                    "text": text(&args.join(" ")),
                }));
            }
            "Runtime.exceptionThrown" => {
                let d = p.get("exceptionDetails").cloned().unwrap_or(Value::Null);
                let message = d
                    .pointer("/exception/description")
                    .and_then(Value::as_str)
                    .or_else(|| d.get("text").and_then(Value::as_str))
                    .unwrap_or("exception");
                self.console.push(json!({
                    "at": at, "kind": "exception", "target": tid, "level": "error",
                    "text": text(message),
                    "url": d.get("url").and_then(Value::as_str).map(safe_url),
                }));
            }
            "Log.entryAdded" => {
                let entry = p.get("entry").cloned().unwrap_or(Value::Null);
                self.console.push(json!({
                    "at": at, "kind": "log", "target": tid,
                    "level": entry.get("level").and_then(Value::as_str).unwrap_or("info"),
                    "source": entry.get("source").and_then(Value::as_str),
                    "text": text(entry.get("text").and_then(Value::as_str).unwrap_or("")),
                    "url": entry.get("url").and_then(Value::as_str).map(safe_url),
                }));
            }
            "Network.requestWillBeSent" => {
                self.network.push(json!({
                    "at": at, "kind": "request", "target": tid,
                    "requestId": p.get("requestId").and_then(Value::as_str).map(text),
                    "method": p.pointer("/request/method").and_then(Value::as_str).map(text),
                    "url": p.pointer("/request/url").and_then(Value::as_str).map(safe_url),
                    "resourceType": p.get("type").and_then(Value::as_str),
                }));
            }
            "Network.responseReceived" => {
                self.network.push(json!({
                    "at": at, "kind": "response", "target": tid,
                    "requestId": p.get("requestId").and_then(Value::as_str).map(text),
                    "status": p.pointer("/response/status").and_then(Value::as_i64),
                    "mimeType": p.pointer("/response/mimeType").and_then(Value::as_str).map(text),
                    "url": p.pointer("/response/url").and_then(Value::as_str).map(safe_url),
                    "resourceType": p.get("type").and_then(Value::as_str),
                }));
            }
            "Network.loadingFailed" => {
                self.network.push(json!({
                    "at": at, "kind": "failed", "target": tid,
                    "requestId": p.get("requestId").and_then(Value::as_str).map(text),
                    "error": p.get("errorText").and_then(Value::as_str).map(text),
                    "canceled": p.get("canceled").and_then(Value::as_bool),
                    "blockedReason": p.get("blockedReason").and_then(Value::as_str),
                }));
            }
            _ => {}
        }
    }

    /// Registers newly discovered page targets (popups) and configures newly attached
    /// out-of-process frames.
    pub fn settle_pending(&mut self, bounds: &Bounds) -> Result<(), CdpError> {
        for _ in 0..8 {
            if self.pending_attach.is_empty() && self.pending_children.is_empty() {
                return Ok(());
            }
            for (i, child) in std::mem::take(&mut self.pending_children) {
                let session = child.session.clone();
                match self.configure_child(i, child, bounds) {
                    Ok(()) => {}
                    // A frame that went away before it was configured.
                    Err(CdpError::Remote { .. }) => self.detach_child(i, &session),
                    Err(e) => return Err(e),
                }
            }
            for p in std::mem::take(&mut self.pending_attach) {
                let info = p.get("info").cloned().unwrap_or(Value::Null);
                let known = info
                    .get("targetId")
                    .and_then(Value::as_str)
                    .is_some_and(|t| self.registered(t));
                if !known && self.open_count() >= MAX_TARGETS {
                    // More targets than a session records: the new one is closed, not kept.
                    self.refuse_target(&info, bounds);
                    continue;
                }
                let r = match p.get("sessionId").and_then(Value::as_str) {
                    Some(s) => self.register(&info, s, "popup", bounds).map(Some),
                    None => self.attach(&info, "popup", bounds),
                };
                match r {
                    Ok(_) => {}
                    // A target that went away before registration finished is not an error.
                    Err(CdpError::Remote { .. }) => {}
                    Err(e) => return Err(e),
                }
            }
            self.absorb_events();
        }
        Ok(())
    }

    /// Reads events for at most `budget` and applies them.
    pub fn pump(&mut self, budget: Duration, bounds: &Bounds) -> Result<(), CdpError> {
        let r = self.cdp.pump(budget, Duration::from_millis(5));
        self.absorb_events();
        r?;
        self.settle_pending(bounds)
    }

    fn open_count(&self) -> usize {
        self.targets.iter().filter(|t| !t.closed).count()
    }

    /// Closes a page target the session will not register (it is at [`MAX_TARGETS`]) and
    /// records why with the session's notifications.
    fn refuse_target(&mut self, info: &Value, bounds: &Bounds) {
        let cdp = info.get("targetId").and_then(Value::as_str).unwrap_or("");
        let closed = self
            .call(
                None,
                "Target.closeTarget",
                json!({"targetId": cdp}),
                bounds.call,
            )
            .is_ok();
        let at = self.at();
        self.console.push(json!({
            "at": at, "kind": "targetRefused", "reason": "targetLimit",
            "limit": MAX_TARGETS, "closed": closed,
            "url": info.get("url").and_then(Value::as_str).map(safe_url),
        }));
    }

    /// Forgets closed targets (their references are stale already). Called only between
    /// actions, when no target index is held and no frame awaits configuration.
    pub fn prune_closed(&mut self) {
        if self.pending_children.is_empty() {
            self.targets.retain(|t| !t.closed);
        }
    }

    /// The open (not closed) targets, in registration order.
    pub fn open_targets(&self) -> Vec<String> {
        self.targets
            .iter()
            .filter(|t| !t.closed)
            .map(|t| t.id.clone())
            .collect()
    }

    fn target_index(&self, id: &str) -> Option<usize> {
        self.targets.iter().position(|t| t.id == id && !t.closed)
    }

    /// The target, CDP frame and hosting CDP session a wire target names, when its document
    /// revision is current.
    pub fn resolve(&self, target: &BrowserTarget) -> Result<(usize, Frame), ActionError> {
        // Host target IDs repeat across sessions (`t1`, ...): a reference names its session.
        if target.session_id != self.session {
            return Err(ActionError::Refused {
                kind: "browser_target_session",
                detail: "the reference belongs to another browser session".into(),
            });
        }
        let Some(i) = self.target_index(target.target_id.as_str()) else {
            return Err(ActionError::Stale {
                kind: "browser_target",
                detail: format!("target {} is not open", target.target_id.as_str()),
            });
        };
        let t = &self.targets[i];
        if t.crashed {
            return Err(ActionError::Stale {
                kind: "browser_target",
                detail: format!("target {} crashed", t.id),
            });
        }
        let Some(frame) = t.frames.iter().find(|f| f.host == target.frame_id.as_str()) else {
            return Err(ActionError::Stale {
                kind: "browser_frame",
                detail: format!(
                    "frame {} is not in target {}",
                    target.frame_id.as_str(),
                    t.id
                ),
            });
        };
        if target.document_revision.get() != t.document_revision {
            return Err(ActionError::Stale {
                kind: "browser_document",
                detail: format!(
                    "document revision {} is not current ({})",
                    target.document_revision.as_str(),
                    t.document_revision
                ),
            });
        }
        Ok((i, frame.clone()))
    }

    /// The exact DOM node a snapshot named, when its snapshot and document are still current.
    pub fn resolve_node(&self, node: &BrowserNode) -> Result<(usize, NodeRef), ActionError> {
        let (i, _) = self.resolve(&node.target)?;
        let t = &self.targets[i];
        let Some(s) = t
            .snapshots
            .iter()
            .find(|s| s.revision == node.snapshot_revision.get())
        else {
            return Err(ActionError::Stale {
                kind: "browser_snapshot",
                detail: format!(
                    "snapshot {} of target {} is no longer retained",
                    node.snapshot_revision.as_str(),
                    t.id
                ),
            });
        };
        if s.document_revision != t.document_revision || s.frame != node.target.frame_id.as_str() {
            return Err(ActionError::Stale {
                kind: "browser_snapshot",
                detail: "the snapshot belongs to another document or frame".into(),
            });
        }
        match s.nodes.get(node.node_id.as_str()) {
            Some(n) => Ok((i, n.clone())),
            None => Err(ActionError::Stale {
                kind: "browser_node",
                detail: format!(
                    "node {} is not in snapshot {}",
                    node.node_id.as_str(),
                    s.revision
                ),
            }),
        }
    }

    /// The current URL and title of a target (from its navigation history; no page script).
    pub fn location(&mut self, i: usize, bounds: &Bounds) -> Result<(String, String), CdpError> {
        let s = self.targets[i].cdp_session.clone();
        let h = self.call(
            Some(&s),
            "Page.getNavigationHistory",
            json!({}),
            bounds.call,
        )?;
        let idx = h.get("currentIndex").and_then(Value::as_u64).unwrap_or(0) as usize;
        let entry = h
            .get("entries")
            .and_then(Value::as_array)
            .and_then(|e| e.get(idx))
            .cloned()
            .unwrap_or(Value::Null);
        let url = entry
            .get("url")
            .and_then(Value::as_str)
            .map(safe_url)
            .unwrap_or_else(|| self.targets[i].url.clone());
        let title = entry
            .get("title")
            .and_then(Value::as_str)
            .map(text)
            .unwrap_or_default();
        Ok((url, title))
    }

    /// Takes an accessibility snapshot of one frame: a new snapshot revision with a node map,
    /// and its bounded, redacted evidence document.
    pub fn snapshot(
        &mut self,
        target: &BrowserTarget,
        bounds: &Bounds,
    ) -> Result<(BrowserTarget, String, String, Vec<u8>), ActionError> {
        // Frames attached by the last request are configured before the document is read.
        self.settle_pending(bounds).map_err(ActionError::Before)?;
        let (i, frame) = self.resolve(target)?;
        // Read on the session hosting the frame's document: an out-of-process frame has its own.
        let s = frame.session.clone();
        let tree = self
            .call(
                Some(&s),
                "Accessibility.getFullAXTree",
                json!({"frameId": frame.cdp}),
                bounds.call,
            )
            .map_err(ActionError::Before)?;
        let (url, title) = self.location(i, bounds).map_err(ActionError::Before)?;
        // The document may have changed while the tree was read.
        let (i, _) = self.resolve(target)?;
        let ax = tree
            .get("nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut host_of: HashMap<String, String> = HashMap::new();
        let mut nodes = HashMap::new();
        let mut out = Vec::new();
        let mut truncated = false;
        for n in &ax {
            if n.get("ignored").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if out.len() >= NODE_LIMIT {
                truncated = true;
                break;
            }
            let Some(ax_id) = n.get("nodeId").and_then(Value::as_str) else {
                continue;
            };
            let host = format!("n{}", out.len() + 1);
            host_of.insert(ax_id.to_owned(), host.clone());
            let role = n
                .pointer("/role/value")
                .and_then(Value::as_str)
                .unwrap_or("generic")
                .to_owned();
            if let Some(b) = n.get("backendDOMNodeId").and_then(Value::as_i64) {
                nodes.insert(
                    host.clone(),
                    NodeRef {
                        backend: b,
                        role: role.clone(),
                        session: s.clone(),
                    },
                );
            }
            out.push((n.clone(), host, role));
        }
        let mut list = Vec::with_capacity(out.len());
        for (n, host, role) in out {
            let states: Vec<Value> = n
                .get("properties")
                .and_then(Value::as_array)
                .map(|ps| {
                    ps.iter()
                        .filter_map(|p| {
                            let name = p.get("name").and_then(Value::as_str)?;
                            let v = p.pointer("/value/value")?;
                            matches!(
                                name,
                                "focused"
                                    | "disabled"
                                    | "checked"
                                    | "expanded"
                                    | "selected"
                                    | "required"
                                    | "invalid"
                                    | "readonly"
                                    | "pressed"
                                    | "focusable"
                                    | "editable"
                                    | "multiselectable"
                            )
                            .then(|| json!({"name": name, "value": v}))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let parent = n
                .get("parentId")
                .and_then(Value::as_str)
                .and_then(|p| host_of.get(p))
                .cloned();
            list.push(json!({
                "nodeId": host,
                "parentId": parent,
                "role": role,
                "name": n.pointer("/name/value").and_then(Value::as_str).map(text),
                "value": n.pointer("/value/value").map(|v| match v {
                    Value::String(s) => Value::String(text(s)),
                    other => other.clone(),
                }),
                "description": n.pointer("/description/value").and_then(Value::as_str).map(text),
                "states": states,
                "actionable": nodes.contains_key(&host),
            }));
        }
        let t = &mut self.targets[i];
        t.next_snapshot += 1;
        let revision = t.next_snapshot;
        if t.snapshots.len() >= SNAPSHOTS_KEPT {
            t.snapshots.pop_front();
        }
        t.snapshots.push_back(Snapshot {
            revision,
            frame: target.frame_id.as_str().to_owned(),
            document_revision: t.document_revision,
            nodes,
        });
        let frames: Vec<Value> = t
            .frames
            .iter()
            .enumerate()
            .map(|(k, f)| {
                json!({
                    "frameId": f.host,
                    "main": k == 0,
                    "outOfProcess": f.session != t.cdp_session,
                })
            })
            .collect();
        let wire = t.wire(&self.session, target.frame_id.as_str());
        let mut doc = json!({
            "sessionId": self.session.as_str(),
            "targetId": t.id,
            "frameId": target.frame_id.as_str(),
            "documentRevision": t.document_revision,
            "snapshotRevision": revision,
            "url": url,
            "title": title,
            "frames": frames,
            "framesTruncated": t.frames_cut,
            "nodes": list,
            "truncated": truncated,
        });
        let mut bytes = doc.to_string().into_bytes();
        while bytes.len() > EVIDENCE_LIMIT * 8 && !doc["nodes"].as_array().is_none_or(Vec::is_empty)
        {
            // Bound the document: keep the first half of the nodes until it fits.
            let nodes = doc["nodes"].as_array_mut().expect("nodes");
            let keep = nodes.len() / 2;
            nodes.truncate(keep);
            doc["truncated"] = json!(true);
            bytes = doc.to_string().into_bytes();
        }
        Ok((wire, url, title, bytes))
    }

    /// A PNG screenshot of a target's main frame (the whole page when `full_page`, clipped to
    /// [`CAPTURE_EDGE`]). Returns the bytes and whether the capture was clipped.
    pub fn screenshot(
        &mut self,
        target: &BrowserTarget,
        full_page: bool,
        bounds: &Bounds,
    ) -> Result<(Vec<u8>, bool), ActionError> {
        let (i, _) = self.resolve(target)?;
        if target.frame_id.as_str() != self.targets[i].main_frame() {
            return Err(ActionError::Refused {
                kind: "browser_frame",
                detail: "screenshots capture the main frame of a target".into(),
            });
        }
        let s = self.targets[i].cdp_session.clone();
        let mut params = json!({"format": "png"});
        let mut clipped = false;
        if full_page {
            let m = self
                .call(Some(&s), "Page.getLayoutMetrics", json!({}), bounds.call)
                .map_err(ActionError::Before)?;
            let size = m
                .get("cssContentSize")
                .or_else(|| m.get("contentSize"))
                .cloned()
                .unwrap_or(Value::Null);
            let w = size.get("width").and_then(Value::as_f64).unwrap_or(0.0);
            let h = size.get("height").and_then(Value::as_f64).unwrap_or(0.0);
            clipped = w > CAPTURE_EDGE || h > CAPTURE_EDGE;
            params = json!({
                "format": "png",
                "captureBeyondViewport": true,
                "clip": {"x": 0, "y": 0, "width": w.clamp(1.0, CAPTURE_EDGE), "height": h.clamp(1.0, CAPTURE_EDGE), "scale": 1},
            });
        }
        let shot = self
            .call(
                Some(&s),
                "Page.captureScreenshot",
                params,
                bounds.navigation,
            )
            .map_err(ActionError::Before)?;
        let data = shot.get("data").and_then(Value::as_str).unwrap_or("");
        if data.len() > SCREENSHOT_LIMIT / 3 * 4 + 4 {
            return Err(ActionError::Refused {
                kind: "browser_screenshot",
                detail: "the screenshot exceeds its size bound".into(),
            });
        }
        let bytes = decode_base64(data).ok_or_else(|| {
            ActionError::Before(CdpError::Protocol("screenshot data is not base64".into()))
        })?;
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(ActionError::Before(CdpError::Protocol(
                "screenshot is not a PNG image".into(),
            )));
        }
        // The capture must still show the document the caller named.
        self.resolve(target)?;
        Ok((bytes, clipped))
    }

    /// Opens a new page target and registers it explicitly.
    pub fn open_target(&mut self, bounds: &Bounds) -> Result<usize, ActionError> {
        if self.open_count() >= MAX_TARGETS {
            return Err(ActionError::Refused {
                kind: "browser_target_limit",
                detail: format!(
                    "the session has {MAX_TARGETS} open targets; navigate one of them or close one"
                ),
            });
        }
        let created = self
            .call(
                None,
                "Target.createTarget",
                json!({"url": "about:blank"}),
                bounds.call,
            )
            .map_err(ActionError::After)?;
        let Some(cdp) = created.get("targetId").and_then(Value::as_str) else {
            return Err(ActionError::After(CdpError::Protocol(
                "createTarget returned no target".into(),
            )));
        };
        let info = json!({"targetId": cdp, "type": "page", "url": "about:blank"});
        // Auto-attach may have registered it already (as a popup of nobody).
        if let Some(i) = self.targets.iter().position(|t| t.cdp_target == cdp) {
            self.targets[i].origin = "opened";
            return Ok(i);
        }
        self.pending_attach
            .retain(|p| p.pointer("/info/targetId").and_then(Value::as_str) != Some(cdp));
        self.attach(&info, "opened", bounds)
            .map_err(ActionError::After)?
            .ok_or_else(|| ActionError::After(CdpError::Protocol("target not attached".into())))
    }

    /// Navigates target `i` to `url` and waits (bounded) for its load.
    pub fn navigate(
        &mut self,
        i: usize,
        url: &str,
        bounds: &Bounds,
        closing: &dyn Fn() -> bool,
    ) -> Result<(), ActionError> {
        let s = self.targets[i].cdp_session.clone();
        let r = self
            .call(Some(&s), "Page.navigate", json!({"url": url}), bounds.call)
            .map_err(|e| match e {
                CdpError::Remote { .. } => ActionError::Before(e),
                other => ActionError::After(other),
            })?;
        if let Some(err) = r.get("errorText").and_then(Value::as_str) {
            self.pump(Duration::from_millis(50), bounds)
                .map_err(ActionError::After)?;
            return Err(ActionError::Navigation(text(err)));
        }
        let Some(loader) = r.get("loaderId").and_then(Value::as_str).map(str::to_owned) else {
            // A same-document navigation: no new document to wait for.
            return Ok(());
        };
        let deadline = Instant::now() + bounds.navigation;
        while !self.targets[i].loaded.contains(&loader) {
            if closing() {
                return Err(ActionError::Closing);
            }
            if Instant::now() >= deadline || self.targets[i].crashed {
                // The navigation was dispatched; the snapshot shows what is there now.
                break;
            }
            self.pump(Duration::from_millis(50), bounds)
                .map_err(ActionError::After)?;
        }
        Ok(())
    }

    /// Performs one input action and returns what it touched (the target index).
    pub fn input(&mut self, action: &BrowserInput, bounds: &Bounds) -> Result<usize, ActionError> {
        self.settle_pending(bounds).map_err(ActionError::Before)?;
        match action {
            BrowserInput::Click { node, button } => {
                let (i, n) = self.resolve_node(node)?;
                // Pointer input goes to the top-level page; DOM reads go to the node's session.
                let s = self.targets[i].cdp_session.clone();
                // A node without a box is refused before anything changes the page.
                let quads = self
                    .call(
                        Some(&n.session),
                        "DOM.getContentQuads",
                        json!({"backendNodeId": n.backend}),
                        bounds.call,
                    )
                    .map_err(ActionError::Before)?;
                if centre(&quads).is_none() {
                    return Err(ActionError::Refused {
                        kind: "browser_node",
                        detail: "the node has no visible box to click".into(),
                    });
                }
                // From the scroll on, the page may have changed: every failure is post-dispatch.
                let (x, y) = self
                    .place_click(i, &n, node, bounds)
                    .map_err(post_dispatch)?;
                let button = match button {
                    MouseButton::Primary => "left",
                    MouseButton::Secondary => "right",
                    MouseButton::Middle => "middle",
                };
                for (kind, count) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 1)]
                {
                    self.call(
                        Some(&s),
                        "Input.dispatchMouseEvent",
                        json!({"type": kind, "x": x, "y": y, "button": if count == 0 { "none" } else { button }, "clickCount": count}),
                        bounds.call,
                    )
                    .map_err(ActionError::After)?;
                }
                Ok(i)
            }
            BrowserInput::Text { node, text: t } => {
                let (i, n) = self.resolve_node(node)?;
                // Focus the node in its own document; text goes to the focused frame.
                let s = self.targets[i].cdp_session.clone();
                self.call(
                    Some(&n.session),
                    "DOM.focus",
                    json!({"backendNodeId": n.backend}),
                    bounds.call,
                )
                .map_err(before_or_after)?;
                // Focus ran page handlers: from here on every failure is post-dispatch.
                self.resolve_node(node).map_err(post_dispatch)?;
                self.call(
                    Some(&s),
                    "Input.insertText",
                    json!({"text": t.0}),
                    bounds.call,
                )
                .map_err(ActionError::After)?;
                Ok(i)
            }
            BrowserInput::Key {
                target,
                key,
                modifiers,
            } => {
                let (i, frame) = self.resolve(target)?;
                let Some(k) = key_of(&key.0) else {
                    return Err(ActionError::Refused {
                        kind: "browser_key",
                        detail: format!("unsupported key {}", text(&key.0)),
                    });
                };
                // Keys reach the frame holding keyboard focus, which must be the targeted one.
                self.require_focus(&frame, bounds)?;
                let mask = modifiers.0.iter().fold(0, |m, x| {
                    m | match x {
                        KeyModifier::Alt => 1,
                        KeyModifier::Control => 2,
                        KeyModifier::Meta => 4,
                        KeyModifier::Shift => 8,
                    }
                });
                let s = self.targets[i].cdp_session.clone();
                let printable = mask & 0b110 == 0 && k.text.is_some();
                let mut down = json!({
                    "type": if printable { "keyDown" } else { "rawKeyDown" },
                    "key": k.key, "code": k.code, "windowsVirtualKeyCode": k.vk,
                    "modifiers": mask,
                });
                if printable {
                    down["text"] = json!(k.text);
                }
                self.call(Some(&s), "Input.dispatchKeyEvent", down, bounds.call)
                    .map_err(ActionError::After)?;
                self.call(
                    Some(&s),
                    "Input.dispatchKeyEvent",
                    json!({"type": "keyUp", "key": k.key, "code": k.code, "windowsVirtualKeyCode": k.vk, "modifiers": mask}),
                    bounds.call,
                )
                .map_err(ActionError::After)?;
                Ok(i)
            }
            BrowserInput::Scroll { target, x, y } => {
                let (i, frame) = self.resolve(target)?;
                let s = self.targets[i].cdp_session.clone();
                // A point of the top-level viewport whose hit lands in the requested frame.
                let (px, py) = self.scroll_point(i, &frame, bounds)?;
                self.resolve(target)?;
                self.call(
                    Some(&s),
                    "Input.dispatchMouseEvent",
                    json!({"type": "mouseWheel", "x": px, "y": py, "deltaX": x, "deltaY": y}),
                    bounds.call,
                )
                .map_err(ActionError::After)?;
                Ok(i)
            }
        }
    }

    /// Refuses unless keyboard focus is in `frame`: the root of its accessibility tree is
    /// `focused` exactly while its document is the focused one (a frame nested in it holding
    /// focus leaves it unfocused). Nothing is sent to the page.
    fn require_focus(&mut self, frame: &Frame, bounds: &Bounds) -> Result<(), ActionError> {
        let tree = self
            .call(
                Some(&frame.session),
                "Accessibility.getFullAXTree",
                json!({"depth": 0, "frameId": frame.cdp}),
                bounds.call,
            )
            .map_err(ActionError::Before)?;
        let focused = tree
            .get("nodes")
            .and_then(Value::as_array)
            .and_then(|ns| ns.iter().find(|n| n.get("parentId").is_none()))
            .and_then(|root| root.get("properties").and_then(Value::as_array))
            .is_some_and(|ps| {
                ps.iter().any(|p| {
                    p.get("name").and_then(Value::as_str) == Some("focused")
                        && p.pointer("/value/value").and_then(Value::as_bool) == Some(true)
                })
            });
        if focused {
            Ok(())
        } else {
            Err(ActionError::Refused {
                kind: "browser_focus",
                detail: format!(
                    "keyboard focus is not in frame {}; focus a node in it first",
                    frame.host
                ),
            })
        }
    }

    /// The out-of-process frames between `session` and its target's page, nearest first: each
    /// hop's own session, owner element, owner content quad and the map from the frame's own
    /// viewport into the embedding document's local-root viewport. `reveal` scrolls each owner
    /// into view first (a click); a scroll reveals nothing it was not asked to move.
    fn hops(
        &mut self,
        i: usize,
        session: &str,
        reveal: bool,
        bounds: &Bounds,
    ) -> Result<Vec<Hop>, ActionError> {
        let mut out = Vec::new();
        let mut current = session.to_owned();
        for _ in 0..16 {
            if self.targets[i].is_page(Some(&current)) {
                return Ok(out);
            }
            let Some(child) = self.targets[i].child_of_session(&current).cloned() else {
                return Err(ActionError::Stale {
                    kind: "browser_frame",
                    detail: "the node's out-of-process frame is no longer attached".into(),
                });
            };
            let (owner, content, map) =
                self.owner_box(&child.parent, &child.frame, reveal, bounds)?;
            out.push(Hop {
                session: current,
                owner,
                content,
                map,
            });
            current = child.parent;
        }
        Err(ActionError::Refused {
            kind: "browser_frame",
            detail: "out-of-process frames nest too deeply".into(),
        })
    }

    /// The owner element of `frame` in the document hosted by `session`, its content quad, and
    /// the map from the frame's viewport into that document's local-root viewport (the owner's
    /// full transform: translation, scale, rotation and skew).
    fn owner_box(
        &mut self,
        session: &str,
        frame: &str,
        reveal: bool,
        bounds: &Bounds,
    ) -> Result<(i64, Vec<f64>, Affine), ActionError> {
        // Reading moves nothing: only a revealing scroll makes a lost answer an unknown outcome.
        let err = if reveal {
            before_or_after
        } else {
            ActionError::Before
        };
        let owner = self
            .call(
                Some(session),
                "DOM.getFrameOwner",
                json!({"frameId": frame}),
                bounds.call,
            )
            .map_err(err)?;
        let Some(owner) = owner.get("backendNodeId").and_then(Value::as_i64) else {
            return Err(ActionError::Refused {
                kind: "browser_frame",
                detail: "the frame has no owner element".into(),
            });
        };
        if reveal {
            self.call(
                Some(session),
                "DOM.scrollIntoViewIfNeeded",
                json!({"backendNodeId": owner}),
                bounds.call,
            )
            .map_err(err)?;
        }
        let model = self
            .call(
                Some(session),
                "DOM.getBoxModel",
                json!({"backendNodeId": owner}),
                bounds.call,
            )
            .map_err(err)?;
        let quad = |k: &str| -> Vec<f64> {
            model
                .pointer(&format!("/model/{k}"))
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_f64).collect())
                .unwrap_or_default()
        };
        let (content, border) = (quad("content"), quad("border"));
        let size = |k: &str| {
            model
                .pointer(&format!("/model/{k}"))
                .and_then(Value::as_f64)
        };
        let no_box = || ActionError::Refused {
            kind: "browser_frame",
            detail: "the frame owner has no box".into(),
        };
        if content.len() != 8 || border.len() != 8 {
            return Err(no_box());
        }
        let (Some(width), Some(height)) = (size("width"), size("height")) else {
            return Err(no_box());
        };
        let map = Affine::of_owner(&content, &border, width, height)?;
        Ok((owner, content, map))
    }

    /// A top-level viewport point at which a wheel event reaches `frame`: inside the visible
    /// part of the frame (the viewport intersected with each owner box on the way down), and
    /// confirmed by hit-testing every local root on the way (each must hit the next owner, the
    /// last the frame itself). A frame scrolled out of view, clipped or covered is refused;
    /// only reads are sent.
    fn scroll_point(
        &mut self,
        i: usize,
        frame: &Frame,
        bounds: &Bounds,
    ) -> Result<(f64, f64), ActionError> {
        let page = self.targets[i].cdp_session.clone();
        let m = self
            .call(Some(&page), "Page.getLayoutMetrics", json!({}), bounds.call)
            .map_err(ActionError::Before)?;
        let vp = m
            .get("cssLayoutViewport")
            .or_else(|| m.get("layoutViewport"))
            .cloned()
            .unwrap_or(Value::Null);
        let w = vp
            .get("clientWidth")
            .and_then(Value::as_f64)
            .unwrap_or(800.0);
        let h = vp
            .get("clientHeight")
            .and_then(Value::as_f64)
            .unwrap_or(600.0);
        // Local roots from the page down to the frame's session, each with its offset and the
        // owner element (in its parent) through which it is entered.
        let mut hops = self.hops(i, &frame.session, false, bounds)?;
        hops.reverse();
        let mut visible = (0.0, 0.0, w, h);
        let mut roots = vec![(page, Affine::IDENTITY, None::<i64>)];
        for hop in hops {
            let parent = roots.last().map_or(Affine::IDENTITY, |r| r.1);
            visible = intersect(visible, rect(&parent.quad(&hop.content)));
            roots.last_mut().expect("a root").2 = Some(hop.owner);
            roots.push((hop.session, parent.then(&hop.map), None));
        }
        // A frame inside its local root's document (not the root's own frame) has an owner there.
        let root_frame = match self.targets[i].child_of_session(&frame.session) {
            Some(c) => c.frame.clone(),
            None => self.targets[i].frames[0].cdp.clone(),
        };
        if frame.cdp != root_frame {
            let (_, content, _) = self.owner_box(&frame.session, &frame.cdp, false, bounds)?;
            let root = roots.last().map_or(Affine::IDENTITY, |r| r.1);
            visible = intersect(visible, rect(&root.quad(&content)));
        }
        let (x0, y0, x1, y1) = visible;
        if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
            return Err(ActionError::Refused {
                kind: "browser_frame",
                detail: format!("frame {} is not visible in the viewport", frame.host),
            });
        }
        let (cx, cy, qw, qh) = (
            (x0 + x1) / 2.0,
            (y0 + y1) / 2.0,
            (x1 - x0) / 4.0,
            (y1 - y0) / 4.0,
        );
        for (px, py) in [
            (cx, cy),
            (cx - qw, cy - qh),
            (cx + qw, cy - qh),
            (cx - qw, cy + qh),
            (cx + qw, cy + qh),
        ] {
            if self.reaches(&roots, &frame.cdp, px, py, bounds)? {
                return Ok((px, py));
            }
        }
        Err(ActionError::Refused {
            kind: "browser_frame",
            detail: format!("frame {} is covered where it is visible", frame.host),
        })
    }

    /// Whether a top-level point hits through each local root's next owner into `frame` (each
    /// root is hit-tested at the point mapped back into its own viewport).
    fn reaches(
        &mut self,
        roots: &[(String, Affine, Option<i64>)],
        frame: &str,
        x: f64,
        y: f64,
        bounds: &Bounds,
    ) -> Result<bool, ActionError> {
        for (session, map, next) in roots {
            let Some((lx, ly)) = map.invert(x, y) else {
                return Ok(false);
            };
            let hit = self
                .call(
                    Some(session),
                    "DOM.getNodeForLocation",
                    json!({"x": lx.floor(), "y": ly.floor(), "includeUserAgentShadowDOM": false, "ignorePointerEventsNone": true}),
                    bounds.call,
                )
                .map_err(ActionError::Before)?;
            let ok = match next {
                Some(owner) => hit.get("backendNodeId").and_then(Value::as_i64) == Some(*owner),
                None => hit.get("frameId").and_then(Value::as_str) == Some(frame),
            };
            if !ok {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The map from the viewport of the document hosted by `session` into its target's
    /// top-level viewport: each out-of-process frame owner's full transform, composed up to the
    /// page.
    fn frame_map(
        &mut self,
        i: usize,
        session: &str,
        bounds: &Bounds,
    ) -> Result<Affine, ActionError> {
        let hops = self.hops(i, session, true, bounds)?;
        Ok(hops
            .iter()
            .rev()
            .fold(Affine::IDENTITY, |top, h| top.then(&h.map)))
    }

    /// Scrolls a node into view and computes its click point in the top-level viewport.
    fn place_click(
        &mut self,
        i: usize,
        n: &NodeRef,
        node: &BrowserNode,
        bounds: &Bounds,
    ) -> Result<(f64, f64), ActionError> {
        self.call(
            Some(&n.session),
            "DOM.scrollIntoViewIfNeeded",
            json!({"backendNodeId": n.backend}),
            bounds.call,
        )
        .map_err(ActionError::After)?;
        let quads = self
            .call(
                Some(&n.session),
                "DOM.getContentQuads",
                json!({"backendNodeId": n.backend}),
                bounds.call,
            )
            .map_err(ActionError::After)?;
        let (x, y) = centre(&quads).ok_or_else(|| {
            ActionError::After(CdpError::Protocol(
                "the node lost its box after scrolling".into(),
            ))
        })?;
        let to_top = self.frame_map(i, &n.session, bounds)?;
        self.resolve_node(node)?;
        Ok(to_top.apply(x, y))
    }

    /// Sets the files of a file input after re-checking that the node is still exactly that
    /// file input (and that it accepts several files when several are given).
    pub fn set_files(
        &mut self,
        node: &BrowserNode,
        paths: &[String],
        bounds: &Bounds,
    ) -> Result<usize, ActionError> {
        let (i, n) = self.resolve_node(node)?;
        let s = n.session.clone();
        let d = self
            .call(
                Some(&s),
                "DOM.describeNode",
                json!({"backendNodeId": n.backend}),
                bounds.call,
            )
            .map_err(before_or_after)?;
        let name = d
            .pointer("/node/nodeName")
            .and_then(Value::as_str)
            .unwrap_or("");
        let attrs: Vec<String> = d
            .pointer("/node/attributes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let attr = |k: &str| {
            attrs
                .chunks(2)
                .find(|c| c[0].eq_ignore_ascii_case(k))
                .map(|c| c.get(1).cloned().unwrap_or_default())
        };
        if !name.eq_ignore_ascii_case("input")
            || !attr("type").is_some_and(|t| t.eq_ignore_ascii_case("file"))
        {
            return Err(ActionError::Refused {
                kind: "browser_node",
                detail: "the node is not a file input".into(),
            });
        }
        if paths.len() > 1 && attr("multiple").is_none() {
            return Err(ActionError::Refused {
                kind: "browser_node",
                detail: "the file input accepts one file".into(),
            });
        }
        self.resolve_node(node)?;
        self.call(
            Some(&s),
            "DOM.setFileInputFiles",
            json!({"files": paths, "backendNodeId": n.backend}),
            bounds.call,
        )
        .map_err(ActionError::After)?;
        Ok(i)
    }

    /// What an action observably did: waits up to the settle bound for its effects, then
    /// describes the target before/after (URL, document revision, new targets, dialogs).
    pub fn observe_after(
        &mut self,
        i: usize,
        before: (String, i64, usize, u64),
        action: Value,
        bounds: &Bounds,
    ) -> Result<Vec<u8>, CdpError> {
        let deadline = Instant::now() + bounds.settle;
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(25), bounds)?;
        }
        let t = &self.targets[i];
        let (url_before, rev_before, targets_before, console_before) = before;
        let new_targets: Vec<&str> = self.targets[targets_before..]
            .iter()
            .map(|t| t.id.as_str())
            .collect();
        let doc = json!({
            "sessionId": self.session.as_str(),
            "targetId": t.id,
            "action": action,
            "urlBefore": url_before,
            "urlAfter": t.url,
            "documentRevisionBefore": rev_before,
            "documentRevisionAfter": t.document_revision,
            "navigated": t.document_revision != rev_before,
            "targetCrashed": t.crashed,
            "newTargets": new_targets,
            "consoleNotices": self.console.total.saturating_sub(console_before),
            "outcome": "input delivered; the workflow outcome is what this observation shows, not a pass",
        });
        Ok(doc.to_string().into_bytes())
    }

    /// The state an action observation compares against.
    pub fn before(&self, i: usize) -> (String, i64, usize, u64) {
        let t = &self.targets[i];
        (
            t.url.clone(),
            t.document_revision,
            self.targets.len(),
            self.console.total,
        )
    }
}

/// An error observed after the navigation or input was sent: whatever it is, the effect may
/// have happened, so it is never reported as "nothing was sent".
pub fn post_dispatch(e: ActionError) -> ActionError {
    match e {
        ActionError::After(e) => ActionError::After(e),
        ActionError::Closing => ActionError::Closing,
        ActionError::Before(e) => ActionError::After(e),
        ActionError::Stale { detail, .. }
        | ActionError::Refused { detail, .. }
        | ActionError::Navigation(detail) => {
            ActionError::After(CdpError::Protocol(format!("after the action: {detail}")))
        }
    }
}

/// One out-of-process frame boundary on the way from a document up to its page.
struct Hop {
    /// The frame's own session.
    session: String,
    owner: i64,
    /// The owner's content quad in the parent's local-root viewport.
    content: Vec<f64>,
    /// From the frame's viewport into the parent's local-root viewport.
    map: Affine,
}

/// An affine map of viewport points: `origin + x * u + y * v`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    origin: (f64, f64),
    u: (f64, f64),
    v: (f64, f64),
}
impl Affine {
    pub const IDENTITY: Affine = Affine {
        origin: (0.0, 0.0),
        u: (1.0, 0.0),
        v: (0.0, 1.0),
    };

    /// The map from a frame's viewport into its owner's document: the owner's content quad is
    /// the image of the frame's viewport, whose size is the content box's untransformed size
    /// (the border box's `width` and `height` scaled by the content-to-border edge ratio, which
    /// any linear transform preserves). A perspective transform (a quad that is not a
    /// parallelogram) has no such map and is refused.
    pub fn of_owner(
        content: &[f64],
        border: &[f64],
        width: f64,
        height: f64,
    ) -> Result<Affine, ActionError> {
        let p = |q: &[f64], k: usize| (q[2 * k], q[2 * k + 1]);
        let len = |a: (f64, f64), b: (f64, f64)| (b.0 - a.0).hypot(b.1 - a.1);
        let (c0, c1, c2, c3) = (p(content, 0), p(content, 1), p(content, 2), p(content, 3));
        let (b0, b1, b3) = (p(border, 0), p(border, 1), p(border, 3));
        let (bw, bh, cw, ch) = (len(b0, b1), len(b0, b3), len(c0, c1), len(c0, c3));
        if [bw, bh, cw, ch, width, height].iter().any(|d| *d < 1e-6) {
            return Err(ActionError::Refused {
                kind: "browser_frame",
                detail: "the frame owner has no box".into(),
            });
        }
        let fourth = (c1.0 + c3.0 - c0.0, c1.1 + c3.1 - c0.1);
        if len(fourth, c2) > 1.0 {
            return Err(ActionError::Refused {
                kind: "browser_frame",
                detail: "the frame owner has a perspective transform".into(),
            });
        }
        let (lw, lh) = (cw / bw * width, ch / bh * height);
        Ok(Affine {
            origin: c0,
            u: ((c1.0 - c0.0) / lw, (c1.1 - c0.1) / lw),
            v: ((c3.0 - c0.0) / lh, (c3.1 - c0.1) / lh),
        })
    }

    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.origin.0 + x * self.u.0 + y * self.v.0,
            self.origin.1 + x * self.u.1 + y * self.v.1,
        )
    }

    /// `self` after `inner`: a point of `inner`'s source mapped through both.
    pub fn then(&self, inner: &Affine) -> Affine {
        let lin = |d: (f64, f64)| {
            (
                d.0 * self.u.0 + d.1 * self.v.0,
                d.0 * self.u.1 + d.1 * self.v.1,
            )
        };
        Affine {
            origin: self.apply(inner.origin.0, inner.origin.1),
            u: lin(inner.u),
            v: lin(inner.v),
        }
    }

    /// The source point mapped to `(x, y)`, if the map is invertible.
    pub fn invert(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let det = self.u.0 * self.v.1 - self.u.1 * self.v.0;
        if det.abs() < 1e-12 {
            return None;
        }
        let (dx, dy) = (x - self.origin.0, y - self.origin.1);
        Some((
            (dx * self.v.1 - dy * self.v.0) / det,
            (self.u.0 * dy - self.u.1 * dx) / det,
        ))
    }

    /// A quad mapped point by point.
    fn quad(&self, q: &[f64]) -> Vec<f64> {
        (0..4)
            .flat_map(|k| {
                let (x, y) = self.apply(q[2 * k], q[2 * k + 1]);
                [x, y]
            })
            .collect()
    }
}

/// The bounding rectangle `(x0, y0, x1, y1)` of a quad.
fn rect(quad: &[f64]) -> (f64, f64, f64, f64) {
    let xs = [quad[0], quad[2], quad[4], quad[6]];
    let ys = [quad[1], quad[3], quad[5], quad[7]];
    let min = |v: [f64; 4]| v.into_iter().fold(f64::INFINITY, f64::min);
    let max = |v: [f64; 4]| v.into_iter().fold(f64::NEG_INFINITY, f64::max);
    (min(xs), min(ys), max(xs), max(ys))
}

fn intersect(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    (a.0.max(b.0), a.1.max(b.1), a.2.min(b.2), a.3.min(b.3))
}

fn before_or_after(e: CdpError) -> ActionError {
    match e {
        CdpError::Remote { .. } | CdpError::Protocol(_) => ActionError::Before(e),
        other => ActionError::After(other),
    }
}

fn collect_frames(children: Option<&Value>, out: &mut Vec<String>) {
    for c in children.and_then(Value::as_array).into_iter().flatten() {
        if let Some(id) = c.pointer("/frame/id").and_then(Value::as_str) {
            out.push(id.to_owned());
        }
        collect_frames(c.get("childFrames"), out);
    }
}

fn remote_text(v: &Value) -> String {
    match v.get("value") {
        Some(Value::String(s)) => s.clone(),
        Some(other) if !other.is_null() => other.to_string(),
        _ => v
            .get("description")
            .and_then(Value::as_str)
            .or_else(|| v.get("type").and_then(Value::as_str))
            .unwrap_or("")
            .to_owned(),
    }
}

/// The centre of the first content quad.
fn centre(quads: &Value) -> Option<(f64, f64)> {
    let q = quads.get("quads")?.as_array()?.first()?.as_array()?;
    if q.len() != 8 {
        return None;
    }
    let v: Vec<f64> = q.iter().filter_map(Value::as_f64).collect();
    if v.len() != 8 {
        return None;
    }
    Some((
        (v[0] + v[2] + v[4] + v[6]) / 4.0,
        (v[1] + v[3] + v[5] + v[7]) / 4.0,
    ))
}

/// A key the input action can press.
pub struct Key {
    pub key: String,
    pub code: String,
    pub vk: i64,
    pub text: Option<String>,
}

/// Named keys and single printable characters.
pub fn key_of(name: &str) -> Option<Key> {
    let named = |key: &str, code: &str, vk: i64, text: Option<&str>| {
        Some(Key {
            key: key.into(),
            code: code.into(),
            vk,
            text: text.map(str::to_owned),
        })
    };
    match name {
        "Enter" => named("Enter", "Enter", 13, Some("\r")),
        "Tab" => named("Tab", "Tab", 9, None),
        "Escape" => named("Escape", "Escape", 27, None),
        "Backspace" => named("Backspace", "Backspace", 8, None),
        "Delete" => named("Delete", "Delete", 46, None),
        "Space" | " " => named(" ", "Space", 32, Some(" ")),
        "ArrowUp" => named("ArrowUp", "ArrowUp", 38, None),
        "ArrowDown" => named("ArrowDown", "ArrowDown", 40, None),
        "ArrowLeft" => named("ArrowLeft", "ArrowLeft", 37, None),
        "ArrowRight" => named("ArrowRight", "ArrowRight", 39, None),
        "Home" => named("Home", "Home", 36, None),
        "End" => named("End", "End", 35, None),
        "PageUp" => named("PageUp", "PageUp", 33, None),
        "PageDown" => named("PageDown", "PageDown", 34, None),
        _ => {
            let mut chars = name.chars();
            let c = chars.next()?;
            if chars.next().is_some() || c.is_control() {
                return None;
            }
            let (code, vk) = if c.is_ascii_alphabetic() {
                (
                    format!("Key{}", c.to_ascii_uppercase()),
                    c.to_ascii_uppercase() as i64,
                )
            } else if c.is_ascii_digit() {
                (format!("Digit{c}"), c as i64)
            } else {
                (String::new(), 0)
            };
            Some(Key {
                key: c.to_string(),
                code,
                vk,
                text: Some(c.to_string()),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame owner's content and border quads (5 px border, 3 px padding, 316 x 216 border
    /// box) as Chrome 156 reports them under `scale(0.5)` and `rotate(30deg)` with origin `0 0`;
    /// the frame-local point (120, 50) is where Chrome's own hit-testing found the frame's button.
    #[test]
    fn a_frame_owner_transform_maps_frame_points_into_its_document() {
        let cases = [
            (
                [204.0, 104.0, 354.0, 104.0, 354.0, 204.0, 204.0, 204.0],
                [200.0, 100.0, 358.0, 100.0, 358.0, 208.0, 200.0, 208.0],
                (264.0, 129.0),
            ),
            (
                [
                    202.928_207,
                    110.928_2,
                    462.735_809,
                    260.928_192,
                    362.735_809,
                    434.133_270,
                    102.928_2,
                    284.133_270,
                ],
                [
                    200.0,
                    100.0,
                    473.664_032,
                    258.0,
                    365.664_032,
                    445.061_493,
                    92.0,
                    287.061_493,
                ],
                (281.851_246, 214.229_464),
            ),
        ];
        for (content, border, (ex, ey)) in cases {
            let m = Affine::of_owner(&content, &border, 316.0, 216.0).unwrap();
            let (x, y) = m.apply(120.0, 50.0);
            assert!((x - ex).abs() < 0.01 && (y - ey).abs() < 0.01, "({x}, {y})");
            let (lx, ly) = m.invert(x, y).unwrap();
            assert!((lx - 120.0).abs() < 1e-6 && (ly - 50.0).abs() < 1e-6);
            // Composition: the same frame nested in an untransformed frame at (10, 20).
            let outer = Affine {
                origin: (10.0, 20.0),
                ..Affine::IDENTITY
            };
            assert_eq!(outer.then(&m).apply(120.0, 50.0), (x + 10.0, y + 20.0));
        }
        // A perspective transform has no affine map: it is refused, never approximated.
        let skewed = [0.0, 0.0, 100.0, 10.0, 90.0, 60.0, 0.0, 50.0];
        assert!(matches!(
            Affine::of_owner(&skewed, &skewed, 100.0, 50.0),
            Err(ActionError::Refused { .. })
        ));
    }

    #[test]
    fn evidence_urls_drop_credentials_fragments_and_secret_parameters() {
        assert_eq!(
            safe_url("https://user:pw@example.test/a/b?q=1&token=abc&Api-Key=z#frag"),
            "https://example.test/a/b?q=1&token=[REDACTED]&Api-Key=[REDACTED]"
        );
        assert_eq!(safe_url("about:blank"), "about:blank");
        assert_eq!(safe_url("http://h.test/p#x?token=1"), "http://h.test/p");
        assert_eq!(
            safe_url("https://h.test/cb?code=xyz&state=ok"),
            "https://h.test/cb?code=[REDACTED]&state=ok"
        );
    }

    #[test]
    fn keys_map_named_and_printable_and_refuse_others() {
        assert_eq!(key_of("Enter").unwrap().vk, 13);
        let a = key_of("a").unwrap();
        assert_eq!((a.code.as_str(), a.text.as_deref()), ("KeyA", Some("a")));
        assert!(key_of("F13").is_none());
        assert!(key_of("\u{7}").is_none());
        assert!(key_of("").is_none());
    }

    #[test]
    fn notices_are_bounded_and_report_drops() {
        let mut n = Notices::default();
        for i in 0..(NOTICE_LIMIT + 3) {
            n.push(json!({"i": i}));
        }
        assert_eq!(n.dropped, 3);
        let batch = n.take();
        let (doc, truncated) = batch.document(&Id::new("s").unwrap(), "Console", false);
        assert!(truncated);
        let v: Value = serde_json::from_slice(&doc).unwrap();
        assert_eq!(v["dropped"], 3);
        assert_eq!(v["entries"].as_array().unwrap().len(), NOTICE_LIMIT);
        assert!(n.is_empty());
        // A failed publication gives the batch back ahead of newer entries.
        n.push(json!({"i": "newer"}));
        n.restore(batch);
        assert_eq!(
            n.dropped,
            3 + 1,
            "still bounded: the oldest one is dropped and counted"
        );
        let again = n.take();
        assert_eq!(again.entries.last().unwrap()["i"], "newer");
        assert_eq!(again.dropped, 4);
    }

    #[test]
    fn an_oversize_backlog_is_published_over_several_observations_never_cut() {
        let mut n = Notices::default();
        let big = "x".repeat(1000);
        for i in 0..1000 {
            n.push(json!({"i": i, "text": big}));
        }
        let mut seen = 0;
        while !n.is_empty() {
            let b = n.take();
            assert!(!b.entries.is_empty());
            let (doc, _) = b.document(&Id::new("s").unwrap(), "Console", false);
            assert!(doc.len() <= EVIDENCE_LIMIT);
            seen += b.entries.len();
        }
        assert_eq!(seen, 1000);
    }
}
