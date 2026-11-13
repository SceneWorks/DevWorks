//! Owned browser sessions end to end on the real SQLite store, the real process owner and a
//! deterministic fake Chrome DevTools endpoint (DW-047): session lifecycle through the process
//! owner, frame/document/node revisions, input and upload with explicit file grants, popups,
//! dialogs, downloads and permissions, console/network evidence and redaction, unknown outcomes
//! and replay, authority, crash, shutdown and boot recovery. The fake speaks the CDP message
//! shapes the client relies on; the browser "process" is a real owned sleeping tree, so its
//! launch, stop and recovery are the real ones. No browser, network or inference is used.
//!
//! An opt-in test drives an installed Chromium-family browser instead
//! (`DEVWORKS_BROWSER_TESTS=1`, optionally `DEVWORKS_BROWSER=<path>`); it is skipped otherwise.
mod common;

use common::*;
use devworks_adapters::browser::NativeBrowserPlatform;
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::fs::artifacts::NativeArtifactFiles;
use devworks_adapters::fs::sources::HostArtifactSources;
use devworks_adapters::process::NativeProcessHost;
use devworks_core::authority::HostAuthority;
use devworks_core::authority::service::bind_principal;
use devworks_core::context::{InvocationContext, PrincipalKind};
use devworks_core::ports::browser::MAX_TARGETS;
use devworks_core::ports::browser::{
    BrowserAsset, BrowserFault, BrowserPlatform, CdpTransport, Profile, Qualification,
};
use devworks_core::ports::process::{
    ControlChannel, NativeIdentity, Probe, ProcessHost, ProcessSpec,
};
use devworks_core::ports::storage::*;
use devworks_core::ports::{AuthorityDenial, AuthorityPort, AuthorityRequest, Authorization};
use devworks_core::protocol::provider::ArtifactRef;
use devworks_core::protocol::records::{
    BrowserObservation, BrowserSnapshot, HealthState, ItemState,
};
use devworks_core::protocol::results::{
    DomainCommandReply, DomainDecision, DomainQueryReply, OperationCompletion, OperationResult,
};
use devworks_core::protocol::{
    ActorId, CommandReply, DomainErrorCode, Id, OperationId, OperationState, ProjectId, RetryAdvice,
};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::artifacts::evidence::Evidence;
use devworks_core::services::browser::cdp::encode_base64;
use devworks_core::services::browser::live::MAX_FRAMES;
use devworks_core::services::browser::{BrowserConfig, BrowserService, EFFECT_CONSUMER};
use devworks_core::settings::catalog::AgentRole;
use devworks_engine::browser::BrowserComposition;
use devworks_engine::{Composition, Runtime, RuntimeConfig, ShutdownStage, boot};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Tests of this binary spawn owned process trees; they run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------------------------
// The fake DevTools endpoint

/// One actionable element of a fake page.
#[derive(Clone)]
struct Node {
    backend: i64,
    role: &'static str,
    name: &'static str,
    tag: &'static str,
    attrs: Vec<&'static str>,
    boxed: bool,
}
fn form_nodes() -> Vec<Node> {
    let n = |backend, role, name, tag, attrs: Vec<&'static str>| Node {
        backend,
        role,
        name,
        tag,
        attrs,
        boxed: true,
    };
    let mut note = n(17, "generic", "Note", "DIV", vec![]);
    note.boxed = false;
    vec![
        n(10, "textbox", "Name", "INPUT", vec!["type", "text"]),
        n(11, "button", "Submit", "BUTTON", vec![]),
        n(12, "button", "Attachment", "INPUT", vec!["type", "file"]),
        n(13, "button", "Open popup", "BUTTON", vec![]),
        n(14, "link", "Next page", "A", vec!["href", "/next"]),
        n(15, "button", "Alert", "BUTTON", vec![]),
        n(16, "button", "Download", "BUTTON", vec![]),
        note,
        n(
            18,
            "button",
            "Attachments",
            "INPUT",
            vec!["type", "file", "multiple", ""],
        ),
    ]
}

struct Page {
    target: String,
    session: String,
    frame: String,
    url: String,
    title: String,
    loader: u64,
    nodes: Vec<Node>,
    focused: Option<i64>,
    values: HashMap<i64, String>,
    files: HashMap<i64, Vec<String>>,
    /// Total wheel delta its own document received.
    scrolled: f64,
}

/// An out-of-process frame of a fake page: its own session and document.
struct FakeChild {
    session: String,
    frame: String,
    page: usize,
    clicked: u32,
    focused: bool,
    value: String,
    /// Total wheel delta its document received.
    scrolled: f64,
}

/// A deterministic browser model speaking the CDP shapes the client relies on.
#[derive(Default)]
struct FakeBrowser {
    children: Vec<FakeChild>,
    /// Page sessions that asked to auto-attach their out-of-process frames.
    auto_attach: HashSet<String>,
    pages: Vec<Page>,
    next: u64,
    out: VecDeque<String>,
    closed: bool,
    /// Methods that are received but never answered (an unknown outcome).
    hang: HashSet<String>,
    /// Methods on which the connection drops (no answer).
    drop_on: HashSet<String>,
    /// Every request received, in order.
    log: Vec<Value>,
    protocol: String,
    /// A response withheld until the open dialog is answered.
    blocked: Option<Value>,
    /// An element covers every out-of-process frame owner (hit-testing finds it instead).
    frame_covered: bool,
    /// Out-of-process frame owners are laid out below the viewport.
    frame_hidden: bool,
    /// A CSS scale of out-of-process frame owners (around their top-left corner).
    frame_scale: Option<f64>,
    /// An element covers this node: hit-testing over it finds the cover (backend 99).
    cover: Option<i64>,
    /// Sessions that enabled request interception (`Fetch.enable`).
    fetching: HashSet<String>,
    /// A navigation paused at its request (interception): its request ID, page, URL and the
    /// navigation's own request ID.
    paused: Option<(String, usize, String, Value)>,
    /// The cover has `pointer-events: none`: pointer input passes through it.
    cover_inert: bool,
    /// This node has `pointer-events: none`: pointer input passes through it to the page.
    pointer_none: Option<i64>,
    /// Focusing a page node does not take (a handler moves focus away at once).
    steal_focus: bool,
    /// Reads wait (past their timeout) while set; `reads_stalled` records that one waited.
    stall_reads: bool,
    reads_stalled: bool,
    /// Once: when a page receives this method, it first navigates to this URL (its document is
    /// replaced while the request is answered), then answers from the new document.
    navigate_on: Option<(String, String)>,
    /// One navigation commits without the optional load lifecycle notification.
    suppress_next_load: bool,
    /// Hover opens a same-document cover before a click can press.
    cover_on_move: bool,
}

impl FakeBrowser {
    fn new() -> Self {
        let mut b = Self {
            protocol: "1.3".into(),
            ..Self::default()
        };
        b.open_page("about:blank", None);
        b
    }
    fn emit(&mut self, session: Option<&str>, method: &str, params: Value) {
        let mut m = json!({"method": method, "params": params});
        if let Some(s) = session {
            m["sessionId"] = json!(s);
        }
        self.out.push_back(m.to_string());
    }
    fn reply(&mut self, id: &Value, result: Value) {
        self.out
            .push_back(json!({"id": id, "result": result}).to_string());
    }
    fn error(&mut self, id: &Value, message: &str) {
        self.out.push_back(
            json!({"id": id, "error": {"code": -32000, "message": message}}).to_string(),
        );
    }
    fn open_page(&mut self, url: &str, opener: Option<&str>) -> usize {
        self.next += 1;
        let n = self.next;
        self.pages.push(Page {
            target: format!("T{n}"),
            session: format!("S{n}"),
            frame: format!("T{n}"),
            url: url.into(),
            title: String::new(),
            loader: 0,
            nodes: vec![],
            focused: None,
            values: HashMap::new(),
            files: HashMap::new(),
            scrolled: 0.0,
        });
        let i = self.pages.len() - 1;
        if let Some(o) = opener {
            let info = json!({"targetId": format!("T{n}"), "type": "page", "url": url, "openerId": o, "attached": true});
            self.emit(None, "Target.targetCreated", json!({"targetInfo": info}));
            self.emit(
                None,
                "Target.attachedToTarget",
                json!({"sessionId": format!("S{n}"), "targetInfo": info, "waitingForDebugger": false}),
            );
        }
        i
    }
    fn page_of(&self, session: Option<&str>) -> Option<usize> {
        self.pages
            .iter()
            .position(|p| Some(p.session.as_str()) == session)
    }
    fn load(&mut self, i: usize, url: &str) -> (String, String) {
        let p = &mut self.pages[i];
        p.loader += 1;
        p.url = url.into();
        p.focused = None;
        p.values.clear();
        if url.contains("/form") {
            p.title = "Form".into();
            p.nodes = form_nodes();
        } else if url.contains("/next") {
            p.title = "Next".into();
            p.nodes = vec![];
        } else if url.contains("/oopif") {
            p.title = "Partner".into();
            p.nodes = vec![Node {
                backend: 20,
                role: "Iframe",
                name: "Partner frame",
                tag: "IFRAME",
                attrs: vec!["src", "https://partner.test/inner"],
                boxed: true,
            }];
        } else if url.contains("/popup") {
            p.title = "Popup".into();
            p.nodes = vec![];
        } else {
            p.title = String::new();
            p.nodes = vec![];
        }
        let (s, f, l) = (p.session.clone(), p.frame.clone(), format!("L{}", p.loader));
        self.emit(
            Some(&s),
            "Network.requestWillBeSent",
            json!({"requestId": format!("R{l}"), "request": {"url": url, "method": "GET"}, "type": "Document"}),
        );
        self.emit(
            Some(&s),
            "Network.responseReceived",
            json!({"requestId": format!("R{l}"), "response": {"url": url, "status": 200, "mimeType": "text/html"}, "type": "Document"}),
        );
        self.emit(
            Some(&s),
            "Page.frameNavigated",
            json!({"frame": {"id": f, "url": url, "loaderId": l}}),
        );
        self.children.retain(|c| c.page != i);
        if url.contains("/oopif") {
            let frame = format!("OOP{i}-{l}");
            self.emit(
                Some(&s),
                "Page.frameAttached",
                json!({"frameId": frame, "parentFrameId": f}),
            );
            self.children.push(FakeChild {
                session: format!("C{i}-{l}"),
                frame,
                page: i,
                clicked: 0,
                focused: false,
                value: String::new(),
                scrolled: 0.0,
            });
            if self.auto_attach.contains(&s) {
                self.attach_children(i);
            }
        }
        (f, l)
    }
    /// Announces the out-of-process frames of page `i` on its session (auto-attach, flattened).
    fn attach_children(&mut self, i: usize) {
        let s = self.pages[i].session.clone();
        let frames: Vec<(String, String)> = self
            .children
            .iter()
            .filter(|c| c.page == i)
            .map(|c| (c.session.clone(), c.frame.clone()))
            .collect();
        for (cs, frame) in frames {
            self.emit(
                Some(&s),
                "Target.attachedToTarget",
                json!({"sessionId": cs, "targetInfo": {"targetId": frame, "type": "iframe", "url": "https://partner.test/inner", "attached": true}, "waitingForDebugger": false}),
            );
        }
    }
    /// Requests on an out-of-process frame's own session.
    fn handle_child(&mut self, k: usize, id: &Value, method: &str, p: &Value) {
        let frame = self.children[k].frame.clone();
        match method {
            "Page.getFrameTree" => self.reply(
                id,
                json!({"frameTree": {"frame": {"id": frame, "url": "https://partner.test/inner"}, "childFrames": []}}),
            ),
            "Accessibility.getFullAXTree" => {
                let value = self.children[k].value.clone();
                let focused = self.children[k].focused;
                self.reply(
                    id,
                    json!({"nodes": [
                        {"nodeId": "c1", "ignored": false, "role": {"value": "RootWebArea"}, "name": {"value": "Inner"}, "properties": [{"name": "focused", "value": {"value": focused}}], "backendDOMNodeId": 1},
                        {"nodeId": "c5", "ignored": false, "parentId": "c1", "role": {"value": "button"}, "name": {"value": "Inside"}, "backendDOMNodeId": 5},
                        {"nodeId": "c6", "ignored": false, "parentId": "c1", "role": {"value": "textbox"}, "name": {"value": "Partner field"}, "value": {"value": value}, "backendDOMNodeId": 6},
                    ]}),
                )
            }
            "DOM.getContentQuads" if p["backendNodeId"] == 5 => self.reply(
                id,
                json!({"quads": [[20.0, 10.0, 40.0, 10.0, 40.0, 30.0, 20.0, 30.0]]}),
            ),
            "DOM.focus" if p["backendNodeId"] == 6 => {
                self.children[k].focused = true;
                self.reply(id, json!({}))
            }
            // The frame's document is 100 x 50 at its own origin; its button at (20, 10)-(40, 30).
            "DOM.getNodeForLocation" => {
                let (x, y) = (p["x"].as_f64().unwrap(), p["y"].as_f64().unwrap());
                let node = if (20.0..40.0).contains(&x) && (10.0..30.0).contains(&y) {
                    5
                } else {
                    1
                };
                if (0.0..100.0).contains(&x) && (0.0..50.0).contains(&y) {
                    self.reply(id, json!({"backendNodeId": node, "frameId": frame}))
                } else {
                    self.error(id, "No node found at given location")
                }
            }
            "Accessibility.getPartialAXTree" => {
                let b = p["backendNodeId"].as_i64().unwrap_or(-1);
                let focused = b == 6 && self.children[k].focused;
                self.reply(
                    id,
                    json!({"nodes": [{"nodeId": "p", "backendDOMNodeId": b, "properties": [{"name": "focused", "value": {"value": focused}}]}]}),
                )
            }
            "DOM.scrollIntoViewIfNeeded" | "DOM.getContentQuads" | "DOM.focus" | "DOM.describeNode"
                if !matches!(p["backendNodeId"].as_i64(), Some(5 | 6)) =>
            {
                self.error(id, "No node with given id found")
            }
            _ => self.reply(id, json!({})),
        }
    }
    fn centre(k: usize) -> (f64, f64) {
        (50.0, 100.0 * (k as f64 + 1.0) + 25.0)
    }
    /// The viewport box `(x0, y0, x1, y1)` of the `k`th node (100 x 50 at its centre; a frame
    /// owner moved below the viewport or scaled when asked).
    fn owner_rect(&self, k: usize, backend: i64) -> (f64, f64, f64, f64) {
        let (cx, mut cy) = Self::centre(k);
        let mut s = 1.0;
        if backend == 20 {
            if self.frame_hidden {
                cy += 1000.0;
            }
            s = self.frame_scale.unwrap_or(1.0);
        }
        let (x0, y0) = (cx - 50.0, cy - 25.0);
        (x0, y0, x0 + 100.0 * s, y0 + 50.0 * s)
    }
    /// The boxed node of page `i` under a viewport point.
    fn hit(&self, i: usize, x: f64, y: f64) -> Option<i64> {
        self.pages[i].nodes.iter().enumerate().find_map(|(k, n)| {
            let (x0, y0, x1, y1) = self.owner_rect(k, n.backend);
            (n.boxed && (x0..x1).contains(&x) && (y0..y1).contains(&y)).then_some(n.backend)
        })
    }
    fn handle(&mut self, raw: &str) {
        let m: Value = serde_json::from_str(raw).unwrap();
        self.log.push(m.clone());
        if self.closed {
            return;
        }
        let method = m["method"].as_str().unwrap_or("").to_owned();
        let id = m["id"].clone();
        let session = m["sessionId"].as_str().map(str::to_owned);
        let p = m["params"].clone();
        if self.drop_on.contains(&method) {
            self.closed = true;
            return;
        }
        if self.hang.contains(&method) {
            return;
        }
        if self.navigate_on.as_ref().is_some_and(|(m, _)| *m == method)
            && let Some(i) = self.page_of(session.as_deref())
        {
            let (_, url) = self.navigate_on.take().unwrap();
            self.load(i, &url);
        }
        if let Some(k) = self
            .children
            .iter()
            .position(|c| Some(c.session.as_str()) == session.as_deref())
        {
            return self.handle_child(k, &id, &method, &p);
        }
        let page = self.page_of(session.as_deref());
        if method == "Target.setAutoAttach"
            && let (Some(i), Some(s)) = (page, session.clone())
        {
            self.auto_attach.insert(s);
            self.reply(&id, json!({}));
            return self.attach_children(i);
        }
        if method == "DOM.getFrameOwner"
            && let Some(i) = page
        {
            let owned = self
                .children
                .iter()
                .any(|c| c.page == i && c.frame == p["frameId"].as_str().unwrap_or_default());
            return if owned {
                self.reply(&id, json!({"backendNodeId": 20}))
            } else {
                self.error(&id, "Frame with the given id was not found.")
            };
        }
        match method.as_str() {
            "Browser.getVersion" => {
                let protocol = self.protocol.clone();
                self.reply(&id, json!({"product": "FakeChrome/147.0", "protocolVersion": protocol}))
            }
            "Browser.setPermission" if p["permission"]["name"] == "clipboard-write" => {
                self.error(&id, "Unknown permission")
            }
            "Browser.close" => {
                self.reply(&id, json!({}));
                self.closed = true;
            }
            "Target.getTargets" => {
                let infos: Vec<Value> = self
                    .pages
                    .iter()
                    .map(|p| json!({"targetId": p.target, "type": "page", "url": p.url, "attached": false}))
                    .collect();
                self.reply(&id, json!({"targetInfos": infos}));
            }
            "Target.attachToTarget" => {
                match self.pages.iter().find(|x| x.target == p["targetId"]) {
                    Some(x) => {
                        let s = x.session.clone();
                        self.reply(&id, json!({"sessionId": s}))
                    }
                    None => self.error(&id, "No target with given id found"),
                }
            }
            "Target.createTarget" => {
                let url = p["url"].as_str().unwrap_or("about:blank").to_owned();
                let i = self.open_page(&url, None);
                let info = json!({"targetId": self.pages[i].target, "type": "page", "url": url, "attached": true});
                let s = self.pages[i].session.clone();
                self.emit(None, "Target.attachedToTarget", json!({"sessionId": s, "targetInfo": info}));
                let t = self.pages[i].target.clone();
                self.reply(&id, json!({"targetId": t}));
            }
            "Page.getFrameTree" => {
                let i = page.unwrap();
                let (f, u) = (self.pages[i].frame.clone(), self.pages[i].url.clone());
                self.reply(&id, json!({"frameTree": {"frame": {"id": f, "url": u}, "childFrames": []}}))
            }
            "Fetch.enable" => {
                if let Some(s) = session.clone() {
                    self.fetching.insert(s);
                }
                self.reply(&id, json!({}))
            }
            // Like Chrome, an intercepted request pauses until the client answers it.
            "Fetch.fulfillRequest" | "Fetch.failRequest" => {
                self.reply(&id, json!({}));
                let rid = p["requestId"].as_str().unwrap_or("").to_owned();
                if let Some((pr, i, url, nav)) = self.paused.take() {
                    if pr != rid {
                        self.paused = Some((pr, i, url, nav));
                        return;
                    }
                    let s = self.pages[i].session.clone();
                    if method == "Fetch.failRequest" {
                        let (f, l) = self.load(i, "chrome-error://chromewebdata/");
                        self.reply(&nav, json!({"frameId": f, "loaderId": l, "errorText": "net::ERR_BLOCKED_BY_CLIENT"}));
                        return;
                    }
                    let body = devworks_core::services::browser::cdp::decode_base64(
                        p["body"].as_str().unwrap_or(""),
                    )
                    .unwrap_or_default();
                    let body = String::from_utf8_lossy(&body).into_owned();
                    let (f, l) = self.load(i, &url);
                    if let Some(t) = body.split("<title>").nth(1).and_then(|r| r.split("</title>").next()) {
                        self.pages[i].title = t.to_owned();
                    }
                    self.emit(Some(&s), "Page.lifecycleEvent", json!({"frameId": f, "loaderId": l, "name": "load"}));
                    self.reply(&nav, json!({"frameId": f, "loaderId": l}));
                }
            }
            "Page.navigate" => {
                let i = page.unwrap();
                let url = p["url"].as_str().unwrap_or("").to_owned();
                let intercepted = url.starts_with("https://artifact.devworks.invalid/") || url.starts_with("file:");
                if intercepted && session.as_ref().is_some_and(|s| self.fetching.contains(s)) {
                    let rid = format!("intercept-{}", self.log.len());
                    let (s, f) = (self.pages[i].session.clone(), self.pages[i].frame.clone());
                    self.paused = Some((rid.clone(), i, url.clone(), id.clone()));
                    self.emit(Some(&s), "Fetch.requestPaused", json!({"requestId": rid, "request": {"url": url, "method": "GET"}, "frameId": f, "resourceType": "Document"}));
                    return;
                }
                if url.contains("missing") {
                    let (f, l) = self.load(i, "chrome-error://chromewebdata/");
                    self.reply(&id, json!({"frameId": f, "loaderId": l, "errorText": "net::ERR_NAME_NOT_RESOLVED"}));
                    return;
                }
                let (f, l) = self.load(i, &url);
                if !url.contains("slow") && !std::mem::take(&mut self.suppress_next_load) {
                    let s = self.pages[i].session.clone();
                    self.emit(Some(&s), "Page.lifecycleEvent", json!({"frameId": f, "loaderId": l, "name": "load"}));
                }
                self.reply(&id, json!({"frameId": f, "loaderId": l}));
            }
            "Page.getNavigationHistory" => {
                let i = page.unwrap();
                let (u, t) = (self.pages[i].url.clone(), self.pages[i].title.clone());
                self.reply(&id, json!({"currentIndex": 0, "entries": [{"url": u, "title": t}]}))
            }
            "Accessibility.getFullAXTree" => {
                let i = page.unwrap();
                // The page's document holds keyboard focus unless one of its frames does.
                let focused = !self.children.iter().any(|c| c.page == i && c.focused);
                let mut nodes = vec![json!({"nodeId": "ax1", "ignored": false, "role": {"value": "RootWebArea"}, "name": {"value": self.pages[i].title}, "properties": [{"name": "focused", "value": {"value": focused}}], "backendDOMNodeId": 1})];
                nodes.push(json!({"nodeId": "ax-ignored", "ignored": true, "role": {"value": "none"}, "parentId": "ax1"}));
                for n in &self.pages[i].nodes {
                    let value = self.pages[i].values.get(&n.backend).cloned();
                    nodes.push(json!({
                        "nodeId": format!("ax{}", n.backend), "ignored": false, "parentId": "ax1",
                        "role": {"value": n.role}, "name": {"value": n.name},
                        "value": value.map(|v| json!({"value": v})),
                        "properties": [{"name": "focusable", "value": {"value": true}}],
                        "backendDOMNodeId": n.backend,
                    }));
                }
                self.reply(&id, json!({"nodes": nodes}));
            }
            "Page.captureScreenshot" => {
                let png = b"\x89PNG\r\n\x1a\nfake-image-bytes";
                self.reply(&id, json!({"data": encode_base64(png)}))
            }
            "Page.getLayoutMetrics" => self.reply(
                &id,
                json!({"cssContentSize": {"width": 800, "height": 2000}, "cssLayoutViewport": {"clientWidth": 800, "clientHeight": 600}}),
            ),
            "DOM.scrollIntoViewIfNeeded" | "DOM.focus" | "DOM.getContentQuads"
            | "DOM.describeNode" | "DOM.setFileInputFiles" | "DOM.getBoxModel" => {
                let i = page.unwrap();
                let b = p["backendNodeId"].as_i64().unwrap_or(-1);
                let Some(k) = self.pages[i].nodes.iter().position(|n| n.backend == b) else {
                    return self.error(&id, "No node with given id found");
                };
                let node = self.pages[i].nodes[k].clone();
                match method.as_str() {
                    "DOM.focus" => {
                        if self.steal_focus {
                            return self.reply(&id, json!({}));
                        }
                        self.pages[i].focused = Some(b);
                        for c in self.children.iter_mut().filter(|c| c.page == i) {
                            c.focused = false;
                        }
                        self.reply(&id, json!({}))
                    }
                    "DOM.getBoxModel" => {
                        // Untransformed 100 x 50 boxes without border or padding; a frame
                        // owner's quad carries its scale.
                        let (x0, y0, x1, y1) = self.owner_rect(k, b);
                        let quad = [x0, y0, x1, y0, x1, y1, x0, y1];
                        self.reply(&id, json!({"model": {"content": quad, "border": quad, "width": 100, "height": 50}}))
                    }
                    "DOM.getContentQuads" => {
                        if node.boxed {
                            let (x, y) = Self::centre(k);
                            self.reply(&id, json!({"quads": [[x - 50.0, y - 25.0, x + 50.0, y - 25.0, x + 50.0, y + 25.0, x - 50.0, y + 25.0]]}))
                        } else {
                            self.reply(&id, json!({"quads": []}))
                        }
                    }
                    "DOM.describeNode" => self.reply(
                        &id,
                        json!({"node": {"nodeName": node.tag, "attributes": node.attrs}}),
                    ),
                    "DOM.setFileInputFiles" => {
                        let files: Vec<String> = p["files"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|f| f.as_str().unwrap().to_owned())
                            .collect();
                        self.pages[i].files.insert(b, files);
                        self.reply(&id, json!({}))
                    }
                    _ => self.reply(&id, json!({})),
                }
            }
            "DOM.getNodeForLocation" => {
                let i = page.unwrap();
                let (x, y) = (p["x"].as_f64().unwrap(), p["y"].as_f64().unwrap());
                let frame = self.pages[i].frame.clone();
                let hit = self.hit(i, x, y);
                // `ignorePointerEventsNone` (default false) counts elements pointer input skips.
                let counts_inert = p["ignorePointerEventsNone"].as_bool().unwrap_or(false);
                let backend = match hit {
                    Some(20) if self.frame_covered => 99,
                    Some(b) if self.cover == Some(b) && (!self.cover_inert || counts_inert) => 99,
                    Some(b) if self.pointer_none == Some(b) && !counts_inert => 1,
                    Some(b) => b,
                    None => 1,
                };
                self.reply(&id, json!({"backendNodeId": backend, "frameId": frame}))
            }
            "Accessibility.getPartialAXTree" => {
                let i = page.unwrap();
                let b = p["backendNodeId"].as_i64().unwrap_or(-1);
                let focused = self.pages[i].focused == Some(b)
                    && !self.children.iter().any(|c| c.page == i && c.focused);
                self.reply(
                    &id,
                    json!({"nodes": [{"nodeId": "p", "backendDOMNodeId": b, "properties": [{"name": "focused", "value": {"value": focused}}]}]}),
                )
            }
            "Input.dispatchMouseEvent" if p["type"] == "mouseMoved" && self.cover_on_move => {
                self.cover_on_move = false;
                self.cover = Some(11);
                self.reply(&id, json!({}));
            }
            "Input.dispatchMouseEvent" if p["type"] == "mouseWheel" => {
                let i = page.unwrap();
                let (x, y) = (p["x"].as_f64().unwrap(), p["y"].as_f64().unwrap());
                let delta = p["deltaY"].as_f64().unwrap_or(0.0);
                let inside = self.hit(i, x, y) == Some(20) && !self.frame_covered;
                match self.children.iter_mut().find(|c| c.page == i) {
                    Some(c) if inside => c.scrolled += delta,
                    _ => self.pages[i].scrolled += delta,
                }
                self.reply(&id, json!({}))
            }
            "Input.insertText" => {
                let i = page.unwrap();
                let t = p["text"].as_str().unwrap_or("").to_owned();
                if let Some(c) = self.children.iter_mut().find(|c| c.page == i && c.focused) {
                    c.value.push_str(&t);
                } else if let Some(f) = self.pages[i].focused {
                    let t = p["text"].as_str().unwrap_or("").to_owned();
                    self.pages[i].values.entry(f).or_default().push_str(&t);
                }
                self.reply(&id, json!({}))
            }
            "Input.dispatchMouseEvent" if p["type"] == "mouseReleased" => {
                let i = page.unwrap();
                let y = p["y"].as_f64().unwrap();
                let s = self.pages[i].session.clone();
                // Routed as real pointer input: an element with `pointer-events: none` is skipped.
                let hit = self
                    .hit(i, p["x"].as_f64().unwrap(), y)
                    .filter(|b| self.pointer_none != Some(*b));
                let k = self.pages[i]
                    .nodes
                    .iter()
                    .position(|n| Some(n.backend) == hit)
                    .unwrap_or(0);
                match hit {
                    Some(11) => {
                        let name = self.pages[i].values.get(&10).cloned().unwrap_or_default();
                        self.emit(Some(&s), "Runtime.consoleAPICalled", json!({"type": "log", "args": [{"type": "string", "value": format!("submitted {name} with ghp_abcdefghijklmnopqrstuvwxyz0123456789")}]}));
                        self.emit(Some(&s), "Network.requestWillBeSent", json!({"requestId": "Rsub", "request": {"url": "https://app.test/submit?key=hunter2&page=1#frag", "method": "POST"}, "type": "XHR"}));
                    }
                    Some(13) => {
                        let opener = self.pages[i].target.clone();
                        let j = self.open_page("https://app.test/popup", Some(&opener));
                        self.load(j, "https://app.test/popup");
                    }
                    Some(14) => {
                        let (f, l) = self.load(i, "https://app.test/next");
                        self.emit(Some(&s), "Page.lifecycleEvent", json!({"frameId": f, "loaderId": l, "name": "load"}));
                    }
                    Some(15) => {
                        self.emit(Some(&s), "Page.javascriptDialogOpening", json!({"type": "alert", "message": "Hello", "url": "https://app.test/form"}));
                        // The click is acknowledged only once the dialog is answered.
                        self.blocked = Some(json!({"id": id, "result": {}}));
                        return;
                    }
                    Some(20) => {
                        // Inside the partner frame: map into its document (its owner's scale).
                        let (ox, oy) = (0.0, 100.0 * (k as f64 + 1.0));
                        let scale = self.frame_scale.unwrap_or(1.0);
                        let (fx, fy) = ((p["x"].as_f64().unwrap() - ox) / scale, (y - oy) / scale);
                        if (20.0..=40.0).contains(&fx) && (10.0..=30.0).contains(&fy)
                            && let Some(c) = self.children.iter_mut().find(|c| c.page == i)
                        {
                            c.clicked += 1;
                            let cs = c.session.clone();
                            self.emit(Some(&cs), "Runtime.consoleAPICalled", json!({"type": "log", "args": [{"type": "string", "value": "inside clicked"}]}));
                        }
                    }
                    Some(16) => {
                        self.emit(None, "Browser.downloadWillBegin", json!({"guid": "g1", "url": "https://app.test/file.zip?token=abc", "suggestedFilename": "file.zip"}));
                    }
                    _ => {}
                }
                self.reply(&id, json!({}))
            }
            "Page.handleJavaScriptDialog" => {
                self.reply(&id, json!({}));
                if let Some(b) = self.blocked.take() {
                    self.out.push_back(b.to_string());
                }
            }
            _ => self.reply(&id, json!({})),
        }
    }
}

struct FakeTransport {
    browser: Arc<Mutex<FakeBrowser>>,
}
impl CdpTransport for FakeTransport {
    fn send(&mut self, message: &str) -> Result<(), BrowserFault> {
        let mut b = self.browser.lock().unwrap();
        if b.closed {
            return Err(BrowserFault::Closed("fake browser closed".into()));
        }
        b.handle(message);
        Ok(())
    }
    fn recv(&mut self, timeout: Duration) -> Result<Option<String>, BrowserFault> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let mut b = self.browser.lock().unwrap();
                if b.stall_reads {
                    b.reads_stalled = true;
                    drop(b);
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                if let Some(m) = b.out.pop_front() {
                    return Ok(Some(m));
                }
                if b.closed {
                    return Err(BrowserFault::Closed("fake browser closed".into()));
                }
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(2).min(timeout));
        }
    }
    fn close(&mut self) {}
}

/// The platform: real owned profile directories (the native implementation), fake assets and
/// a fake DevTools endpoint per launched browser, whose "process" is a real sleeping tree.
struct FakePlatform {
    native: NativeBrowserPlatform,
    assets: Mutex<Vec<BrowserAsset>>,
    browsers: Mutex<HashMap<String, Arc<Mutex<FakeBrowser>>>>,
    launched: Mutex<Vec<Vec<String>>>,
    /// Protocol version the next browsers report.
    protocol: Mutex<String>,
    /// Methods the next browsers never answer.
    hang: Mutex<HashSet<String>>,
    /// When set, profile removal does not return until this gate opens.
    removal: Mutex<Option<Arc<Gate>>>,
    /// When set, the next launch does not start until this gate opens (the session's Create
    /// has passed its `Starting` check and holds the session; nothing is launched yet).
    launch: Mutex<Option<Arc<Gate>>>,
    /// The next launches name a program that does not exist (the launch fails, nothing runs).
    fail_launch: std::sync::atomic::AtomicBool,
    /// While set, no owned tree is ever observed empty: every stop is unverified (the trees
    /// are still really killed).
    unverified_stop: Arc<std::sync::atomic::AtomicBool>,
}
/// A step that does not return until released.
#[derive(Default)]
struct Gate {
    entered: std::sync::atomic::AtomicBool,
    open: Mutex<bool>,
    opened: std::sync::Condvar,
}
impl Gate {
    fn pass(&self) {
        self.entered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.opened.wait(open).unwrap();
        }
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.opened.notify_all();
    }
}
impl FakePlatform {
    fn new(root: &Path) -> Self {
        let asset = |id: &str, q| BrowserAsset {
            id: id.into(),
            product: format!("Fake {id}"),
            program: Some(PathBuf::from("/fake/browser")),
            qualification: q,
        };
        Self {
            native: NativeBrowserPlatform::with_candidates(root, vec![], vec![]),
            assets: Mutex::new(vec![
                asset("chrome", Qualification::Present),
                asset("edge", Qualification::Missing("not installed".into())),
                asset(
                    "chromium",
                    Qualification::Unsupported("no build for this target".into()),
                ),
            ]),
            browsers: Mutex::new(HashMap::new()),
            launched: Mutex::new(vec![]),
            protocol: Mutex::new("1.3".into()),
            hang: Mutex::new(HashSet::new()),
            removal: Mutex::new(None),
            launch: Mutex::new(None),
            fail_launch: Default::default(),
            unverified_stop: Default::default(),
        }
    }
    fn browser(&self, session: &Id) -> Arc<Mutex<FakeBrowser>> {
        self.browsers.lock().unwrap()[session.as_str()].clone()
    }
}
fn sleeping_tree() -> ProcessSpec {
    let mut spec = sleeping_spec();
    // As the native platform does: the browser's DevTools channel is a private control pipe.
    spec.control_pipe = Some(devworks_core::ports::process::ControlPipe {
        windows_args: vec![],
    });
    spec
}
fn sleeping_spec() -> ProcessSpec {
    let argv: Vec<String> = if cfg!(windows) {
        ["cmd", "/C", "ping", "-n", "60", "127.0.0.1"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    } else {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "sleep 60 & sleep 60; wait".into(),
        ]
    };
    ProcessSpec::new(&argv).unwrap()
}
/// The native process host, except that while `unverified` is set no tree is ever observed
/// empty, so the process owner cannot verify any stop (the tree is still really killed).
struct StopHookHost {
    unverified: Arc<std::sync::atomic::AtomicBool>,
}
struct StopHookTree {
    inner: Box<dyn devworks_core::ports::process::ProcessTree>,
    unverified: Arc<std::sync::atomic::AtomicBool>,
}
impl devworks_core::ports::process::ProcessTree for StopHookTree {
    fn identity(&self) -> &NativeIdentity {
        self.inner.identity()
    }
    fn request_stop(&mut self) -> Result<bool, devworks_core::ports::process::ProcessError> {
        self.inner.request_stop()
    }
    fn kill(&mut self) -> Result<(), devworks_core::ports::process::ProcessError> {
        self.inner.kill()
    }
    fn poll(
        &mut self,
    ) -> Result<
        devworks_core::ports::process::TreeStatus,
        devworks_core::ports::process::ProcessError,
    > {
        if self.unverified.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(devworks_core::ports::process::TreeStatus {
                empty: false,
                exit_code: None,
                signal: None,
            });
        }
        self.inner.poll()
    }
    fn take_output(
        &mut self,
    ) -> Option<(Box<dyn std::io::Read + Send>, Box<dyn std::io::Read + Send>)> {
        self.inner.take_output()
    }
    fn take_control(&mut self) -> Option<ControlChannel> {
        self.inner.take_control()
    }
    fn root_exited(&mut self) -> Result<bool, devworks_core::ports::process::ProcessError> {
        if self.unverified.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(false);
        }
        self.inner.root_exited()
    }
}
impl ProcessHost for StopHookHost {
    fn platform(&self) -> &'static str {
        NativeProcessHost.platform()
    }
    fn environment(&self) -> Vec<(String, String)> {
        NativeProcessHost.environment()
    }
    fn spawn(
        &self,
        spec: &ProcessSpec,
    ) -> Result<
        Box<dyn devworks_core::ports::process::ProcessTree>,
        devworks_core::ports::process::ProcessError,
    > {
        Ok(Box::new(StopHookTree {
            inner: NativeProcessHost.spawn(spec)?,
            unverified: self.unverified.clone(),
        }))
    }
    fn probe(&self, identity: &NativeIdentity) -> Probe {
        NativeProcessHost.probe(identity)
    }
    fn terminate(&self, identity: &NativeIdentity) -> Probe {
        NativeProcessHost.terminate(identity)
    }
}

impl BrowserPlatform for FakePlatform {
    fn target(&self) -> String {
        "test-target".into()
    }
    fn assets(&self) -> Vec<BrowserAsset> {
        self.assets.lock().unwrap().clone()
    }
    fn create_profile(&self, session: &Id) -> Result<Profile, BrowserFault> {
        self.native.create_profile(session)
    }
    fn remove_profile(&self, session: &Id) -> Result<(), BrowserFault> {
        let gate = self.removal.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.pass();
        }
        self.native.remove_profile(session)
    }
    fn profiles(&self) -> Result<Vec<Id>, BrowserFault> {
        self.native.profiles()
    }
    fn command(&self, _: &BrowserAsset, _: &Profile, args: &[String]) -> ProcessSpec {
        let gate = self.launch.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.pass();
        }
        self.launched.lock().unwrap().push(args.to_vec());
        if self.fail_launch.load(std::sync::atomic::Ordering::SeqCst) {
            let mut spec = sleeping_tree();
            spec.program = "/nonexistent/devworks-fake-browser".into();
            return spec;
        }
        sleeping_tree()
    }
    fn connect(
        &self,
        profile: &Profile,
        control: Option<ControlChannel>,
    ) -> Result<Box<dyn CdpTransport>, BrowserFault> {
        // The browser service always launches with a private control channel; the fake speaks
        // CDP in process instead of over it.
        let Some((writer, mut reader)) = control else {
            return Err(BrowserFault::NotReady("no control pipe".into()));
        };
        let mut b = FakeBrowser::new();
        b.protocol = self.protocol.lock().unwrap().clone();
        b.hang = self.hang.lock().unwrap().clone();
        let b = Arc::new(Mutex::new(b));
        // As with a real browser, the connection ends once every process of the tree holding
        // the control pipe is gone (its end of the pipe reads end-of-file).
        let watched = b.clone();
        std::thread::spawn(move || {
            let _writer = writer;
            let mut buf = [0u8; 64];
            while matches!(std::io::Read::read(&mut reader, &mut buf), Ok(n) if n > 0) {}
            watched.lock().unwrap().closed = true;
        });
        self.browsers
            .lock()
            .unwrap()
            .insert(profile.session_id.as_str().to_owned(), b.clone());
        Ok(Box::new(FakeTransport { browser: b }))
    }
    fn stage_file(
        &self,
        profile: &Profile,
        batch: &str,
        name: &str,
        source: &mut dyn std::io::Read,
        max: u64,
    ) -> Result<PathBuf, BrowserFault> {
        self.native.stage_file(profile, batch, name, source, max)
    }
    fn remove_staged(&self, profile: &Profile, batch: &str) -> Result<(), BrowserFault> {
        self.native.remove_staged(profile, batch)
    }
    fn read_staged(
        &self,
        profile: &Profile,
        path: &Path,
        max: u64,
    ) -> Result<Vec<u8>, BrowserFault> {
        self.native.read_staged(profile, path, max)
    }
}

// ---------------------------------------------------------------------------------------------
// Fixture

/// The production authority over the test store.
struct StoreAuthority {
    store: Arc<dyn ApplicationStore>,
    clock: Arc<dyn devworks_core::ports::Clock>,
}
impl AuthorityPort for StoreAuthority {
    fn authorize(
        &self,
        ctx: &InvocationContext,
        request: &AuthorityRequest<'_>,
    ) -> Result<Authorization, AuthorityDenial> {
        HostAuthority {
            store: &*self.store,
            clock: &*self.clock,
            protected_branches: &[],
        }
        .authorize(ctx, request)
    }
}

struct Fx {
    h: Option<Harness>,
    rt: tokio::runtime::Runtime,
    app: Option<Runtime>,
    platform: Arc<FakePlatform>,
    user: InvocationContext,
    keys: std::sync::atomic::AtomicUsize,
}

fn config() -> BrowserConfig {
    BrowserConfig {
        launch_timeout: Duration::from_secs(5),
        call_timeout: Duration::from_millis(400),
        navigation_timeout: Duration::from_millis(400),
        settle: Duration::from_millis(30),
        close_grace: Duration::from_millis(200),
        close_bound: Duration::from_secs(5),
        stage_max_bytes: 1024 * 1024,
        order_bound: Duration::from_secs(10),
    }
}

fn seed_project(h: &Harness) {
    let c = rusqlite::Connection::open(h.root.join("workspace.sqlite")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch(
        "INSERT OR IGNORE INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES('project-a','A','Active',1,1,0),('project-b','B','Active',1,1,0);
         INSERT OR IGNORE INTO work_items VALUES('root-a','project-a','feature',NULL,NULL,NULL,'Fixture',1,0,1,1,1,'{}');",
    )
    .unwrap();
}

impl Fx {
    fn new() -> Self {
        let h = Harness::new();
        let platform = Arc::new(FakePlatform::new(&h.root));
        Self::boot(h, platform)
    }
    fn boot(h: Harness, platform: Arc<FakePlatform>) -> Self {
        let real: Arc<dyn BrowserPlatform> = platform.clone();
        Self::boot_with(h, platform, real, config())
    }
    /// Boots over `real` (the fake, or the native platform for the opt-in test).
    fn boot_with(
        h: Harness,
        platform: Arc<FakePlatform>,
        real: Arc<dyn BrowserPlatform>,
        browser: BrowserConfig,
    ) -> Self {
        Self::boot_with_source(h, platform, real, browser, false)
    }
    fn boot_with_source(
        h: Harness,
        platform: Arc<FakePlatform>,
        real: Arc<dyn BrowserPlatform>,
        browser: BrowserConfig,
        source: bool,
    ) -> Self {
        let rt = runtime();
        let store = h.open();
        seed_project(&h);
        let user = principal(&*store, "user-a", PrincipalKind::Human);
        let mut composition = composition(
            &h,
            store.clone(),
            real,
            browser,
            platform.unverified_stop.clone(),
        );
        if source {
            composition.git = Some(source_fixture(&h, &*store, user.principal()));
        }
        let app = rt.block_on(boot(composition)).unwrap();
        Self {
            h: Some(h),
            rt,
            app: Some(app),
            platform,
            user,
            keys: Default::default(),
        }
    }
    fn app(&self) -> &Runtime {
        self.app.as_ref().unwrap()
    }
    fn h(&self) -> &Harness {
        self.h.as_ref().unwrap()
    }
    /// Lets the runtime's tasks (delivery, effects) run for a moment.
    fn drive(&self, ms: u64) {
        let d = Duration::from_millis(ms);
        self.rt.block_on(async move { tokio::time::sleep(d).await });
    }
    /// A new boot over the same data root: after an orderly shutdown, or after the previous
    /// boot ended without one (its runtime dropped: the process owner kills its trees).
    fn restart(self, graceful: bool) -> Fx {
        self.restart_with(graceful, |_| {})
    }
    /// [`Self::restart`], running `between` on the closed data root before the next boot.
    fn restart_with(mut self, graceful: bool, between: impl FnOnce(&Harness)) -> Fx {
        if graceful {
            self.shutdown();
        }
        drop(self.app.take());
        let h = self.h.take().unwrap();
        let platform = self.platform.clone();
        drop(self);
        between(&h);
        Fx::boot(h, platform)
    }
    fn svc(&self) -> &Arc<BrowserService> {
        self.app().browser().unwrap()
    }
    /// A request key never used before (also across boots of one data root).
    fn key(&self) -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        self.keys.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!(
            "key-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        )
    }
    fn command(
        &self,
        ctx: &InvocationContext,
        op: &str,
        key: &str,
        body: Value,
    ) -> DomainCommandReply {
        let mut body = body;
        body["kind"] = json!(op);
        let frame =
            json!({"mode": "Command", "protocol": "1.0-draft.1", "requestKey": key, "body": body});
        self.app()
            .command(ctx, op, frame.to_string().as_bytes())
            .expect("addressable")
    }
    fn query(&self, ctx: &InvocationContext, op: &str, body: Value) -> DomainQueryReply {
        let mut body = body;
        body["kind"] = json!(op);
        let frame = json!({"mode": "Query", "protocol": "1.0-draft.1", "queryId": self.key(), "body": body});
        self.app()
            .query(ctx, op, frame.to_string().as_bytes())
            .expect("addressable")
    }
    fn result(&self, ctx: &InvocationContext, op: &str, body: Value) -> OperationResult {
        match self.query(ctx, op, body) {
            DomainQueryReply::Result { result, .. } => result,
            other => panic!("{op}: {other:?}"),
        }
    }
    fn fault(
        &self,
        ctx: &InvocationContext,
        op: &str,
        body: Value,
    ) -> devworks_core::protocol::DomainFault {
        match self.query(ctx, op, body) {
            DomainQueryReply::Rejected { fault, .. } => fault,
            other => panic!("{op}: expected a fault, got {other:?}"),
        }
    }
    fn decision(reply: &DomainCommandReply) -> &DomainDecision {
        match reply {
            CommandReply::Recorded { receipt, .. } => &receipt.decision,
            other => panic!("{other:?}"),
        }
    }
    fn accepted(&self, ctx: &InvocationContext, op: &str, body: Value) -> OperationId {
        let reply = self.command(ctx, op, &self.key(), body);
        match Self::decision(&reply) {
            DomainDecision::Accepted { operation_id } => operation_id.clone(),
            other => panic!("{op}: {other:?}"),
        }
    }
    fn refused(
        &self,
        ctx: &InvocationContext,
        op: &str,
        body: Value,
    ) -> devworks_core::protocol::DomainFault {
        let reply = self.command(ctx, op, &self.key(), body);
        match Self::decision(&reply) {
            DomainDecision::Refused { fault } => fault.clone(),
            other => panic!("{op}: expected a refusal, got {other:?}"),
        }
    }
    /// Waits for an accepted operation's completion.
    fn completion(&self, ctx: &InvocationContext, op: &OperationId) -> OperationCompletion {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(c) = self.svc().completion(ctx, op).expect("visible") {
                c.validate().expect("valid completion");
                return c;
            }
            assert!(
                Instant::now() < deadline,
                "operation {op:?} did not complete: {:?}",
                read(&**self.app().store(), {
                    let o = op.clone();
                    move |s| s.operations().operation(&o)
                })
            );
            self.drive(10);
        }
    }
    fn succeeded(&self, ctx: &InvocationContext, op: &OperationId) -> OperationResult {
        match self.completion(ctx, op) {
            OperationCompletion::Succeeded { result, .. } => result,
            other => panic!("expected success: {other:?}"),
        }
    }
    fn create(&self, ctx: &InvocationContext, scope: Value) -> Id {
        let op = self.accepted(
            ctx,
            "browser.sessions",
            json!({"action": {"kind": "Create", "scope": scope, "browserAssetId": "chrome"}}),
        );
        match self.succeeded(ctx, &op) {
            OperationResult::BrowserSessions { value } => {
                let s = &value.items.0[0];
                assert!(matches!(s.state, ItemState::Healthy), "{s:?}");
                s.id.clone()
            }
            other => panic!("{other:?}"),
        }
    }
    fn session(&self, id: &Id) -> StoredBrowserSession {
        let id = id.clone();
        read(&**self.app().store(), move |s| {
            s.browser().browser_session(&id)
        })
        .expect("session")
    }
    fn navigate(
        &self,
        ctx: &InvocationContext,
        session: &Id,
        target: Option<&str>,
        url: &str,
    ) -> OperationCompletion {
        let s = self.session(session);
        let mut body = json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "location": {"kind": "Remote", "url": url}});
        if let Some(t) = target {
            body["targetId"] = json!(t);
        }
        let op = self.accepted(ctx, "browser.navigate", body);
        self.completion(ctx, &op)
    }
    fn snapshot(&self, c: OperationCompletion) -> BrowserSnapshot {
        match c {
            OperationCompletion::Succeeded {
                result: OperationResult::BrowserNavigate { value },
                ..
            } => value,
            other => panic!("{other:?}"),
        }
    }
    fn inspect(&self, ctx: &InvocationContext, target: &Value) -> BrowserSnapshot {
        match self.result(ctx, "browser.inspect", json!({"target": target})) {
            OperationResult::BrowserInspect { value } => value,
            other => panic!("{other:?}"),
        }
    }
    fn input(&self, ctx: &InvocationContext, session: &Id, action: Value) -> OperationCompletion {
        let s = self.session(session);
        let op = self.accepted(
            ctx,
            "browser.input",
            json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "action": action}),
        );
        self.completion(ctx, &op)
    }
    fn observation(&self, c: OperationCompletion) -> BrowserObservation {
        match c {
            OperationCompletion::Succeeded {
                result:
                    OperationResult::BrowserInput { value } | OperationResult::BrowserUpload { value },
                ..
            } => value,
            other => panic!("{other:?}"),
        }
    }
    /// The bytes of an artifact the user may see.
    fn bytes(&self, r: &ArtifactRef) -> Vec<u8> {
        let mut read = self
            .app()
            .artifacts()
            .open_exact_version(&**self.app().store(), &self.user, r)
            .expect("readable");
        let mut out = vec![];
        std::io::Read::read_to_end(&mut read.reader, &mut out).unwrap();
        out
    }
    fn json(&self, r: &ArtifactRef) -> Value {
        serde_json::from_slice(&self.bytes(r)).unwrap()
    }
    fn close(&self, ctx: &InvocationContext, session: &Id) -> StoredBrowserSession {
        let s = self.session(session);
        let op = self.accepted(
            ctx,
            "browser.sessions",
            json!({"action": {"kind": "Close", "sessionId": session.as_str(), "expectedRevision": s.revision.to_string()}}),
        );
        self.succeeded(ctx, &op);
        self.session(session)
    }
    fn identity(&self, session: &Id) -> NativeIdentity {
        let p = self.session(session).process_id.expect("launched");
        let rec = read(&**self.app().store(), move |s| s.resources().process(&p))
            .expect("process record");
        NativeIdentity {
            pid: rec.pid.unwrap() as u32,
            birth: rec.birth.unwrap(),
            group: rec.group,
        }
    }
    fn profile_root(&self, session: &Id) -> PathBuf {
        self.h()
            .root
            .join("browser")
            .join("sessions")
            .join(session.as_str())
    }
    fn agent(&self, name: &str, role: AgentRole, project: Option<&str>) -> InvocationContext {
        let store = self.app().store().clone();
        let ctx = principal(&*store, name, PrincipalKind::Agent);
        let (u, b) = (
            self.user.clone(),
            NewPrincipalBinding {
                actor: ActorId(id(name)),
                kind: PrincipalKind::Agent,
                role: Some(role),
                project_id: project.map(|p| ProjectId(id(p))),
                tool_families: None,
                bound_by: ActorId(id("user-a")),
            },
        );
        write(&*store, move |tx| {
            bind_principal(tx, &u, b, None)
                .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
        })
        .unwrap();
        match project {
            Some(p) => ctx.with_lineage(Some(ProjectId(id(p))), None, None),
            None => ctx,
        }
    }
    fn opt_in_agent_browser_tools(&self) {
        let store = self.app().store().clone();
        let schema = devworks_core::settings::catalog::descriptor("tools.overrides")
            .unwrap()
            .schema_ref();
        let selected = devworks_core::registry::all()
            .iter()
            .filter(|operation| operation.family == "browser")
            .map(|operation| (format!("devworks/{}", operation.name), json!(true)))
            .collect::<serde_json::Map<String, Value>>();
        write(&*store, move |tx| {
            tx.configuration().append_revision(
                &ConfigScope::Workspace,
                "settings",
                1,
                1,
                json!({"values":{"tools.overrides":{"schema":schema,"value":selected}}}),
            )?;
            Ok(())
        })
        .unwrap();
    }
    fn shutdown(&mut self) {
        if let Some(app) = self.app.take() {
            self.rt.block_on(app.shutdown());
        }
    }
}
impl Drop for Fx {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn composition(
    h: &Harness,
    store: Arc<dyn ApplicationStore>,
    platform: Arc<dyn BrowserPlatform>,
    browser: BrowserConfig,
    unverified_stop: Arc<std::sync::atomic::AtomicBool>,
) -> Composition {
    let artifacts = Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.clone(),
            h.root.with_extension("cache"),
        )),
        Arc::new(HostArtifactSources::new(h.clock(), h.ids.clone())),
        Arc::new(NativeFilesystem::new()),
        h.clock(),
        h.ids.clone(),
    ));
    let mut config = RuntimeConfig::new(id("dispatcher-1"));
    config.collect_interval = Duration::from_secs(3600);
    config.shutdown_grace = Duration::from_millis(200);
    config.shutdown_bound = Duration::from_secs(10);
    // Supervision passes run only when a test asks (BrowserService::tick).
    config.browser_interval = Duration::from_secs(3600);
    Composition {
        packages: None,
        indexing: None,
        store: store.clone(),
        clock: h.clock(),
        ids: h.ids.clone(),
        authority: Arc::new(StoreAuthority {
            store: store.clone(),
            clock: h.clock(),
        }),
        artifacts,
        resources: Arc::new(devworks_core::services::resources::ResourceService::new(
            h.ids.clone(),
            h.clock(),
        )),
        process_host: Arc::new(StopHookHost {
            unverified: unverified_stop,
        }),
        runs: devworks_engine::RunComposition::new(Arc::new(
            devworks_core::services::runs::ControlService::new(h.ids.clone()),
        )),
        power: Arc::new(NoPower),
        diagnostics: Arc::new(
            devworks_core::services::diagnostics::DiagnosticService::new(
                store.clone(),
                h.ids.clone(),
                h.clock(),
                None,
            ),
        ),
        git: None,
        browser: Some(BrowserComposition {
            platform,
            fs: Arc::new(NativeFilesystem::new()),
            app_data_root: Some(h.root.to_string_lossy().into_owned()),
            config: browser,
        }),
        use_cases: UseCases::new().with_queue(),
        consumers: Vec::new(),
        config,
    }
}

struct NoPower;
impl devworks_core::ports::process::PowerPort for NoPower {
    fn prevent_idle_sleep(
        &self,
    ) -> Result<Box<dyn devworks_core::ports::process::PowerAssertion>, String> {
        Err("not in tests".into())
    }
}

fn target_json(s: &BrowserSnapshot) -> Value {
    serde_json::to_value(&s.target).unwrap()
}
fn node(s: &BrowserSnapshot, accessibility: &Value, name: &str) -> Value {
    let n = accessibility["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == name)
        .unwrap_or_else(|| panic!("no node named {name}"));
    let snapshot_revision = accessibility["snapshotRevision"].as_i64().unwrap();
    json!({"target": target_json(s), "nodeId": n["nodeId"], "snapshotRevision": snapshot_revision.to_string()})
}
fn workspace() -> Value {
    json!({"kind": "Workspace"})
}
fn requests(b: &Arc<Mutex<FakeBrowser>>, method: &str) -> usize {
    b.lock()
        .unwrap()
        .log
        .iter()
        .filter(|m| m["method"] == method)
        .count()
}

// ---------------------------------------------------------------------------------------------
// Tests

#[test]
fn a_session_is_launched_driven_inspected_and_closed_through_the_process_owner() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());

    // Launched through the process owner with an owned, empty profile and headless flags.
    let args = fx.platform.launched.lock().unwrap()[0].clone();
    let root = fx.profile_root(&session);
    assert!(args.contains(&"--headless=new".to_owned()), "{args:?}");
    // The DevTools channel is the private pipe: no port, no address, nothing to attach to.
    assert!(args.contains(&"--remote-debugging-pipe".to_owned()));
    assert!(
        !args.iter().any(|a| a.starts_with("--remote-debugging-port")
            || a.starts_with("--remote-debugging-address"))
    );
    assert!(args.contains(&format!(
        "--user-data-dir={}",
        root.join("profile").display()
    )));
    assert!(root.join("profile").is_dir());
    let identity = fx.identity(&session);
    assert_eq!(NativeProcessHost.probe(&identity), Probe::Alive);
    let stored = fx.session(&session);
    assert_eq!(stored.phase, BrowserPhase::Ready);
    assert_eq!(stored.targets, vec!["t1".to_owned()]);

    // Listed for its scope with its owned process.
    match fx.result(
        &user,
        "browser.sessions",
        json!({"scope": workspace(), "page": {"limit": 10}}),
    ) {
        OperationResult::BrowserSessions { value } => {
            assert_eq!(value.items.0.len(), 1);
            assert_eq!(
                value.items.0[0].process_owner_id,
                stored.process_id.clone().unwrap()
            );
        }
        other => panic!("{other:?}"),
    }

    // Navigation yields a snapshot whose evidence is redacted and bounded.
    let snap = fx.snapshot(fx.navigate(
        &user,
        &session,
        Some("t1"),
        "https://app.test/form?token=SECRET123&q=1",
    ));
    assert_eq!(snap.url.0, "https://app.test/form?token=[REDACTED]&q=1");
    assert_eq!(snap.title.0, "Form");
    assert_eq!(snap.target.frame_id.as_str(), "t1-f0");
    let ax = fx.json(&snap.accessibility);
    assert_eq!(
        ax["documentRevision"].as_i64().unwrap(),
        snap.target.document_revision.get()
    );
    assert!(!ax.to_string().contains("SECRET123"));
    assert!(
        ax["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["role"] != "none"),
        "ignored nodes are skipped"
    );

    // DOM/accessibility targets first: type into the textbox, then click Submit.
    let name = node(&snap, &ax, "Name");
    let typed = fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Text", "node": name, "text": "Ada"}),
    ));
    let evidence = fx.json(&typed.evidence);
    assert_eq!(evidence["action"]["kind"], "Text");
    assert_eq!(evidence["action"]["characters"], 3);
    assert!(
        !evidence.to_string().contains("Ada"),
        "typed text is not repeated into evidence"
    );
    let submit = node(&snap, &ax, "Submit");
    let clicked = fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Click", "node": submit, "button": "Primary"}),
    ));
    assert!(clicked.sequence.get() > typed.sequence.get());
    let b = fx.platform.browser(&session);
    assert_eq!(
        b.lock().unwrap().pages[0]
            .values
            .get(&10)
            .map(String::as_str),
        Some("Ada")
    );
    // Re-inspection takes a new snapshot of the same document; both snapshots' nodes resolve.
    let again = fx.inspect(&user, &target_json(&snap));
    let ax2 = fx.json(&again.accessibility);
    assert_eq!(
        again.target.document_revision,
        snap.target.document_revision
    );
    assert!(ax2["snapshotRevision"].as_i64() > ax["snapshotRevision"].as_i64());
    let value = ax2["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "Name")
        .unwrap()["value"]
        .clone();
    assert_eq!(value, "Ada");
    // Keyboard and scroll on the target.
    let t = target_json(&snap);
    fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Key", "target": t, "key": "Enter", "modifiers": ["Shift"]}),
    ));
    fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Scroll", "target": t, "x": 0, "y": 300}),
    ));
    assert_eq!(requests(&b, "Input.dispatchKeyEvent"), 2);

    // Console and network notifications become bounded, redacted evidence.
    let console = match fx.result(
        &user,
        "browser.console",
        json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 50}}),
    ) {
        OperationResult::BrowserConsole { value } => value,
        other => panic!("{other:?}"),
    };
    let text = console
        .items
        .0
        .iter()
        .map(|o| fx.json(&o.evidence).to_string())
        .collect::<String>();
    assert!(text.contains("submitted Ada"), "{text}");
    assert!(
        !text.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
        "{text}"
    );
    let network = match fx.result(
        &user,
        "browser.network",
        json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 50}}),
    ) {
        OperationResult::BrowserNetwork { value } => value,
        other => panic!("{other:?}"),
    };
    let text = network
        .items
        .0
        .iter()
        .map(|o| fx.json(&o.evidence).to_string())
        .collect::<String>();
    assert!(
        text.contains("https://app.test/submit?key=[REDACTED]&page=1"),
        "{text}"
    );
    assert!(!text.contains("hunter2") && !text.contains("SECRET123") && !text.contains("#frag"));
    // Reading after the last sequence returns nothing new; the cursor pages.
    let last = network.items.0.last().unwrap().sequence.clone();
    match fx.result(&user, "browser.network", json!({"sessionId": session.as_str(), "afterSequence": last.as_str(), "page": {"limit": 50}})) {
        OperationResult::BrowserNetwork { value } => assert!(value.items.0.is_empty()),
        other => panic!("{other:?}"),
    }

    // A screenshot is a PNG artifact of the session's scope.
    let shot = match fx.result(
        &user,
        "browser.screenshot",
        json!({"target": target_json(&snap), "fullPage": true}),
    ) {
        OperationResult::BrowserScreenshot { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(shot.media_type, "image/png");
    assert!(fx.bytes(&shot).starts_with(b"\x89PNG"));

    // Close stops the tree through the process owner and removes the profile; what the page
    // reported until then is still published as evidence.
    b.lock().unwrap().emit(
        Some("S1"),
        "Runtime.consoleAPICalled",
        json!({"type": "warn", "args": [{"type": "string", "value": "last words"}]}),
    );
    let closed = fx.close(&user, &session);
    match fx.result(
        &user,
        "browser.console",
        json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 50}}),
    ) {
        OperationResult::BrowserConsole { value } => {
            let text = value
                .items
                .0
                .iter()
                .map(|o| fx.json(&o.evidence).to_string())
                .collect::<String>();
            assert!(text.contains("last words"), "{text}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(closed.phase, BrowserPhase::Closed);
    assert_eq!(NativeProcessHost.probe(&identity), Probe::Gone);
    assert!(!root.exists(), "the profile root is removed");
    assert_eq!(requests(&b, "Browser.close"), 1);
    // Attributed lifecycle history.
    let kinds: Vec<String> = read(&**fx.app().store(), |s| {
        s.history().page(
            &EventFilter::Aggregate {
                kind: "browser_session".into(),
                id: session.as_str().to_owned(),
            },
            0,
            100,
        )
    })
    .events
    .into_iter()
    .map(|e| e.kind)
    .collect();
    for k in [
        "BrowserSessionRequested",
        "BrowserProcessLaunched",
        "BrowserSessionReady",
        "BrowserSessionCloseRequested",
        "BrowserSessionClosed",
    ] {
        assert!(kinds.iter().any(|x| x == k), "{k} in {kinds:?}");
    }
    // A closed session refuses further commands.
    let s = fx.session(&session);
    let fault = fx.refused(&user, "browser.navigate", json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "location": {"kind": "Blank"}}));
    assert_eq!(fault.code, DomainErrorCode::Conflict);
}

#[test]
fn asynchronous_navigation_returns_a_revision_for_nonmutating_inspection_recovery() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let b = fx.platform.browser(&session);
    let navigations = requests(&b, "Page.navigate");
    b.lock().unwrap().emit(
        Some("S1"),
        "Page.frameNavigated",
        json!({"frame":{"id":"T1","url":"https://app.test/next","loaderId":"Lasync"}}),
    );
    let mut target = target_json(&snap);
    let fault = fx.fault(&user, "browser.inspect", json!({"target":target}));
    assert_eq!(fault.code, DomainErrorCode::SourceChanged);
    let current = fault
        .current_revision
        .expect("stale reads must return the observed current revision");
    assert!(current.get() > snap.target.document_revision.get());
    target["documentRevision"] = json!(current.as_str());
    let fresh = fx.inspect(&user, &target);
    assert_eq!(fresh.target.document_revision, current);
    assert_eq!(
        requests(&b, "Page.navigate"),
        navigations,
        "recovery must not navigate"
    );
}

#[test]
fn stale_frame_document_and_node_references_are_refused_never_retargeted() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let submit = node(&snap, &ax, "Submit");
    let next = node(&snap, &ax, "Next page");

    // Navigation inside the page (a link) advances the document revision.
    fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Click", "node": next, "button": "Primary"}),
    ));
    // A node of the previous document is refused at admission, durably.
    let s = fx.session(&session);
    let fault = fx.refused(&user, "browser.input", json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "action": {"kind": "Click", "node": submit, "button": "Primary"}}));
    assert_eq!(fault.code, DomainErrorCode::SourceChanged);
    assert_eq!(fault.retry, RetryAdvice::RefreshAndNewRequest);
    // A stale document revision on a read is refused too (a fresh snapshot is needed).
    let fault = fx.fault(
        &user,
        "browser.inspect",
        json!({"target": target_json(&snap)}),
    );
    assert_eq!(fault.code, DomainErrorCode::SourceChanged);
    // An unknown frame and node are refused.
    let mut wrong = target_json(&snap);
    wrong["frameId"] = json!("t1-f9");
    assert_eq!(
        fx.fault(&user, "browser.inspect", json!({"target": wrong}))
            .code,
        DomainErrorCode::SourceChanged
    );

    // A reference current at admission but stale at execution fails with nothing sent.
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let submit = node(&snap, &ax, "Submit");
    let b = fx.platform.browser(&session);
    let before = requests(&b, "Input.dispatchMouseEvent");
    b.lock().unwrap().emit(
        Some("S1"),
        "Page.frameNavigated",
        json!({"frame": {"id": "T1", "url": "https://app.test/next", "loaderId": "Lx"}}),
    );
    let s = fx.session(&session);
    let op = fx.accepted(&user, "browser.input", json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "action": {"kind": "Click", "node": submit, "button": "Primary"}}));
    match fx.completion(&user, &op) {
        OperationCompletion::Failed { fault, .. } => {
            assert_eq!(fault.code, DomainErrorCode::SourceChanged);
            assert_eq!(fault.target_kind.as_ref().unwrap().0, "browser_document");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        requests(&b, "Input.dispatchMouseEvent"),
        before,
        "no input was sent"
    );

    // A node the current snapshot never named is refused, even within the current document.
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let mut forged = node(&snap, &ax, "Submit");
    forged["nodeId"] = json!("n999");
    let s = fx.session(&session);
    let fault = fx.refused(&user, "browser.input", json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "action": {"kind": "Click", "node": forged, "button": "Primary"}}));
    assert_eq!(
        (fault.code, fault.target_kind.unwrap().0),
        (DomainErrorCode::SourceChanged, "browser_node".to_owned())
    );
    // A stale session revision is a revision conflict.
    let fault = fx.refused(&user, "browser.navigate", json!({"sessionId": session.as_str(), "expectedRevision": "1", "location": {"kind": "Blank"}}));
    assert_eq!(fault.code, DomainErrorCode::RevisionConflict);
    // A node without a box and an unsupported key are refused before any input.
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let before = requests(&b, "Input.dispatchMouseEvent");
    match fx.input(
        &user,
        &session,
        json!({"kind": "Click", "node": node(&snap, &ax, "Note"), "button": "Primary"}),
    ) {
        OperationCompletion::Failed { fault, .. } => {
            assert_eq!(fault.code, DomainErrorCode::InvalidArgument)
        }
        other => panic!("{other:?}"),
    }
    match fx.input(
        &user,
        &session,
        json!({"kind": "Key", "target": target_json(&snap), "key": "F13", "modifiers": []}),
    ) {
        OperationCompletion::Failed { fault, .. } => {
            assert_eq!(fault.target_kind.unwrap().0, "browser_key")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(requests(&b, "Input.dispatchMouseEvent"), before);
}

#[test]
fn popups_dialogs_downloads_and_permissions_are_qualified() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    // The permission policy applied at launch is recorded (one denial the browser refused).
    let ready = read(&**fx.app().store(), |s| {
        s.history().page(
            &EventFilter::Aggregate {
                kind: "browser_session".into(),
                id: session.as_str().to_owned(),
            },
            0,
            100,
        )
    })
    .events
    .into_iter()
    .find(|e| e.kind == "BrowserSessionReady")
    .unwrap();
    let policy = &ready.payload["policy"];
    assert_eq!(policy["downloads"], "denied");
    assert!(
        policy["permissionsDenied"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "geolocation")
    );
    assert_eq!(
        policy["permissionsNotApplied"][0]["permission"],
        "clipboard-write"
    );
    let b = fx.platform.browser(&session);
    assert!(
        b.lock()
            .unwrap()
            .log
            .iter()
            .any(|m| m["method"] == "Browser.setDownloadBehavior"
                && m["params"]["behavior"] == "deny")
    );

    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    // A popup is registered explicitly under the session: its revision and targets change.
    let before = fx.session(&session).revision;
    fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Click", "node": node(&snap, &ax, "Open popup"), "button": "Primary"}),
    ));
    let s = fx.session(&session);
    assert_eq!(s.targets, vec!["t1".to_owned(), "t2".to_owned()]);
    assert!(s.revision > before);
    // The popup can be inspected once its document revision is learned from a fault.
    let popup = json!({"sessionId": session.as_str(), "targetId": "t2", "frameId": "t2-f0", "documentRevision": "1"});
    let popup = match fx.query(&user, "browser.inspect", json!({"target": popup})) {
        DomainQueryReply::Result {
            result: OperationResult::BrowserInspect { value },
            ..
        } => value,
        DomainQueryReply::Rejected { fault, .. } => {
            assert_eq!(fault.code, DomainErrorCode::SourceChanged);
            let mut t = json!({"sessionId": session.as_str(), "targetId": "t2", "frameId": "t2-f0", "documentRevision": "2"});
            for rev in 2..6 {
                t["documentRevision"] = json!(rev.to_string());
                if let DomainQueryReply::Result {
                    result: OperationResult::BrowserInspect { value },
                    ..
                } = fx.query(&user, "browser.inspect", json!({"target": t}))
                {
                    return assert_popup(&fx, &user, &session, value);
                }
            }
            panic!("popup never inspectable");
        }
        other => panic!("{other:?}"),
    };
    assert_popup(&fx, &user, &session, popup);
}

fn assert_popup(fx: &Fx, user: &InvocationContext, session: &Id, popup: BrowserSnapshot) {
    assert_eq!(popup.title.0, "Popup");
    // Dialogs are answered at once (an alert blocks the click until then) and recorded;
    // downloads are denied and recorded.
    let snap = fx.snapshot(fx.navigate(user, session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    fx.observation(fx.input(
        user,
        session,
        json!({"kind": "Click", "node": node(&snap, &ax, "Alert"), "button": "Primary"}),
    ));
    fx.observation(fx.input(
        user,
        session,
        json!({"kind": "Click", "node": node(&snap, &ax, "Download"), "button": "Primary"}),
    ));
    let b = fx.platform.browser(session);
    assert_eq!(requests(&b, "Page.handleJavaScriptDialog"), 1);
    let console = match fx.result(
        user,
        "browser.console",
        json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 50}}),
    ) {
        OperationResult::BrowserConsole { value } => value,
        other => panic!("{other:?}"),
    };
    let text = console
        .items
        .0
        .iter()
        .map(|o| fx.json(&o.evidence).to_string())
        .collect::<String>();
    assert!(text.contains(r#""kind":"popup""#), "{text}");
    assert!(
        text.contains(r#""dialog":"alert""#) && text.contains(r#""handled":"accepted""#),
        "{text}"
    );
    assert!(
        text.contains(r#""kind":"download""#) && text.contains(r#""handled":"denied""#),
        "{text}"
    );
    assert!(!text.contains("token=abc"), "{text}");
}

#[test]
fn uploads_materialize_only_exact_artifacts_the_caller_may_see() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    fx.opt_in_agent_browser_tools();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let publish = |owner: &InvocationContext, title: &str, bytes: &[u8]| -> ArtifactRef {
        let e = Evidence {
            scope: ArtifactScope::Workspace,
            producer: owner.principal().clone(),
            principal_kind: "Human".into(),
            agent_session: None,
            root: None,
            assignment: None,
            title: title.into(),
            media_type: "text/plain".into(),
            tags: vec![],
            source: json!({"kind": "Fixture"}),
        };
        fx.app()
            .artifacts()
            .publish_evidence(&**fx.app().store(), e, bytes, |_, _| Ok(()))
            .unwrap()
            .0
    };
    // A traversal-shaped title cannot leave the staging batch.
    let file = publish(&user, "../../escape.txt", b"upload bytes");
    let attach = node(&snap, &ax, "Attachment");
    let op = fx.accepted(
        &user,
        "browser.upload",
        json!({"node": attach, "files": [file]}),
    );
    let obs = fx.observation(fx.completion(&user, &op));
    assert_eq!(fx.json(&obs.evidence)["action"]["kind"], "Upload");
    let b = fx.platform.browser(&session);
    let staged = b.lock().unwrap().pages[0].files[&12].clone();
    assert_eq!(staged.len(), 1);
    let path = PathBuf::from(&staged[0]);
    let root = fx.profile_root(&session);
    assert!(path.starts_with(&root), "{path:?} under {root:?}");
    assert_eq!(path.parent().unwrap().parent().unwrap(), root);
    assert_eq!(std::fs::read(&path).unwrap(), b"upload bytes");
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        !name.starts_with('.') && !name.contains('/') && !name.contains('\\'),
        "{name}"
    );

    // A reference whose digest does not match its version is refused, and nothing stays staged.
    let mut forged = file.clone();
    forged.sha256 = serde_json::from_value(json!("0".repeat(64))).unwrap();
    let batches_before = std::fs::read_dir(&root).unwrap().count();
    let fault = fx.refused(
        &user,
        "browser.upload",
        json!({"node": attach, "files": [forged]}),
    );
    assert_eq!(fault.code, DomainErrorCode::NotFound);
    assert_eq!(
        std::fs::read_dir(&root).unwrap().count(),
        batches_before,
        "the refused batch is removed"
    );

    // Another principal's workspace artifact is not granted to an agent.
    let agent = fx.agent("agent-w", AgentRole::Worker, None);
    let agent_session = fx.create(&agent, workspace());
    let snap2 =
        fx.snapshot(fx.navigate(&agent, &agent_session, Some("t1"), "https://app.test/form"));
    let ax2 = fx.json(&snap2.accessibility);
    let fault = fx.refused(
        &agent,
        "browser.upload",
        json!({"node": node(&snap2, &ax2, "Attachment"), "files": [file]}),
    );
    assert_eq!(fault.code, DomainErrorCode::NotFound);

    // The node must still be exactly a file input; several files need `multiple`.
    let two = [publish(&user, "a.txt", b"a"), publish(&user, "b.txt", b"b")];
    let op = fx.accepted(
        &user,
        "browser.upload",
        json!({"node": node(&snap, &ax, "Name"), "files": [two[0]]}),
    );
    match fx.completion(&user, &op) {
        OperationCompletion::Failed { fault, .. } => {
            assert_eq!(fault.code, DomainErrorCode::InvalidArgument)
        }
        other => panic!("{other:?}"),
    }
    let op = fx.accepted(
        &user,
        "browser.upload",
        json!({"node": attach, "files": two}),
    );
    assert!(matches!(
        fx.completion(&user, &op),
        OperationCompletion::Failed { .. }
    ));
    let op = fx.accepted(
        &user,
        "browser.upload",
        json!({"node": node(&snap, &ax, "Attachments"), "files": two}),
    );
    fx.observation(fx.completion(&user, &op));
    assert_eq!(b.lock().unwrap().pages[0].files[&18].len(), 2);
}

#[test]
fn an_unknown_input_outcome_holds_the_session_until_a_person_resolves_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    fx.platform
        .hang
        .lock()
        .unwrap()
        .insert("Input.insertText".into());
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let name = node(&snap, &ax, "Name");
    let s = fx.session(&session);
    let body = json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "action": {"kind": "Text", "node": name, "text": "x"}});
    let key = fx.key();
    let reply = fx.command(&user, "browser.input", &key, body.clone());
    let DomainDecision::Accepted { operation_id: op } = Fx::decision(&reply).clone() else {
        panic!("{reply:?}");
    };
    assert!(matches!(
        fx.completion(&user, &op),
        OperationCompletion::OutcomeUnknown { .. }
    ));
    let b = fx.platform.browser(&session);
    assert_eq!(requests(&b, "Input.insertText"), 1);
    // The same request key replays the receipt; nothing is dispatched again.
    let again = fx.command(&user, "browser.input", &key, body);
    assert!(
        matches!(&again, CommandReply::Recorded { replayed: true, .. }),
        "{again:?}"
    );
    assert!(
        matches!(Fx::decision(&again), DomainDecision::Accepted { operation_id } if *operation_id == op)
    );
    fx.drive(200);
    assert_eq!(
        requests(&b, "Input.insertText"),
        1,
        "a replay never redispatches"
    );
    // New input is refused until the uncertain one is reconciled.
    let s = fx.session(&session);
    assert_eq!(s.uncertain_operation_id.as_ref(), Some(&op));
    let fault = fx.refused(&user, "browser.input", json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "action": {"kind": "Key", "target": target_json(&snap), "key": "Tab", "modifiers": []}}));
    assert_eq!(fault.code, DomainErrorCode::OutcomeUnknown);
    assert_eq!(fault.retry, RetryAdvice::ReconcileOperation);
    assert_eq!(
        fault.target_id.as_ref().map(Id::as_str),
        Some(op.0.as_str())
    );
    // Only a person states what happened; an agent cannot.
    let agent = fx.agent("agent-r", AgentRole::Worker, None);
    assert!(fx.svc().resolve_unknown(&agent, &op, true).is_err());
    assert_eq!(
        fx.svc().resolve_unknown(&user, &op, false).unwrap(),
        OperationState::Failed
    );
    let s = fx.session(&session);
    assert!(s.uncertain_operation_id.is_none());
    fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Key", "target": target_json(&snap), "key": "Tab", "modifiers": []}),
    ));
}

#[test]
fn a_lost_connection_during_input_is_unknown_and_settles_the_crashed_session() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let identity = fx.identity(&session);
    let root = fx.profile_root(&session);
    fx.platform
        .browser(&session)
        .lock()
        .unwrap()
        .drop_on
        .insert("Input.dispatchMouseEvent".into());
    let c = fx.input(
        &user,
        &session,
        json!({"kind": "Click", "node": node(&snap, &ax, "Submit"), "button": "Primary"}),
    );
    assert!(
        matches!(c, OperationCompletion::OutcomeUnknown { .. }),
        "{c:?}"
    );
    let s = fx.session(&session);
    assert_eq!(s.phase, BrowserPhase::Failed, "{s:?}");
    assert_eq!(NativeProcessHost.probe(&identity), Probe::Gone);
    assert!(!root.exists());
}

#[test]
fn an_exited_browser_is_detected_and_its_session_settled() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let identity = fx.identity(&session);
    let root = fx.profile_root(&session);
    assert_eq!(NativeProcessHost.terminate(&identity), Probe::Gone);
    let deadline = Instant::now() + Duration::from_secs(10);
    while fx.session(&session).phase == BrowserPhase::Ready {
        assert!(Instant::now() < deadline, "the exit was not detected");
        fx.svc().tick();
        fx.drive(20);
    }
    let s = fx.session(&session);
    assert_eq!(s.phase, BrowserPhase::Failed);
    assert!(s.detail.unwrap().contains("exited"));
    assert!(!root.exists());
    // A crashed renderer invalidates its target's references.
    let other = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &other, Some("t1"), "https://app.test/form"));
    let b = fx.platform.browser(&other);
    b.lock().unwrap().emit(
        None,
        "Target.targetCrashed",
        json!({"targetId": "T1", "status": "crashed"}),
    );
    fx.svc().tick();
    assert_eq!(
        fx.fault(
            &user,
            "browser.inspect",
            json!({"target": target_json(&snap)})
        )
        .code,
        DomainErrorCode::SourceChanged
    );
}

#[test]
fn reviewers_cannot_use_the_browser_and_only_the_owner_or_a_person_drives_a_session() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    fx.opt_in_agent_browser_tools();
    let user = fx.user.clone();
    let project = json!({"kind": "Project", "projectId": "project-a"});
    let worker = fx.agent("agent-a", AgentRole::Worker, Some("project-a"));
    let other = fx.agent("agent-b", AgentRole::Worker, Some("project-a"));
    let reviewer = fx.agent("agent-r", AgentRole::CodeReviewer, Some("project-a"));
    let outsider = fx.agent("agent-o", AgentRole::Worker, Some("project-b"));
    let session = fx.create(&worker, project.clone());
    let s = fx.session(&session);
    assert_eq!(
        s.project_id.as_ref().map(|p| p.0.as_str()),
        Some("project-a")
    );

    // Reviewers are refused every browser operation, reads included.
    let fault = fx.fault(
        &reviewer,
        "browser.sessions",
        json!({"scope": project, "page": {"limit": 10}}),
    );
    assert_eq!(fault.code, DomainErrorCode::PermissionDenied);
    match fx.command(&reviewer, "browser.navigate", &fx.key(), json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "location": {"kind": "Blank"}})) {
        CommandReply::Rejected { .. } => {}
        other => panic!("{other:?}"),
    }
    // Another agent of the project sees the session but cannot drive it.
    match fx.result(
        &other,
        "browser.sessions",
        json!({"scope": project, "page": {"limit": 10}}),
    ) {
        OperationResult::BrowserSessions { value } => assert_eq!(value.items.0.len(), 1),
        other => panic!("{other:?}"),
    }
    let fault = fx.refused(&other, "browser.navigate", json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "location": {"kind": "Blank"}}));
    assert_eq!(
        (fault.code, fault.target_kind.unwrap().0),
        (
            DomainErrorCode::PermissionDenied,
            "browser_session_owner".to_owned()
        )
    );
    // Another project's agent does not see it at all.
    let fault = fx.refused(&outsider, "browser.navigate", json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "location": {"kind": "Blank"}}));
    assert_eq!(
        fault.code,
        DomainErrorCode::NotFound,
        "existence is not disclosed"
    );
    // The owner drives it; evidence is the project's, linked to the owner.
    let snap = fx.snapshot(fx.navigate(&worker, &session, Some("t1"), "https://app.test/form"));
    let version = snap.accessibility.version_id.clone();
    let a = read(&**fx.app().store(), move |s| {
        let v = s.artifacts().version(&version)?.unwrap();
        s.artifacts().artifact(&v.artifact_id)
    })
    .unwrap();
    assert_eq!(a.scope, ArtifactScope::Project(ProjectId(id("project-a"))));
    assert_eq!(a.provenance["producedBy"], "agent-a");
    // The person interrupts by closing it.
    assert_eq!(fx.close(&user, &session).phase, BrowserPhase::Closed);
}

#[test]
fn a_missing_or_unqualified_browser_is_a_visible_limitation_never_a_session() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    for (asset, code) in [
        ("edge", DomainErrorCode::UnsupportedCapability),
        ("chromium", DomainErrorCode::UnsupportedCapability),
        ("netscape", DomainErrorCode::UnsupportedTarget),
    ] {
        let fault = fx.refused(
            &user,
            "browser.sessions",
            json!({"action": {"kind": "Create", "scope": workspace(), "browserAssetId": asset}}),
        );
        assert_eq!(fault.code, code, "{asset}");
        assert_eq!(fault.target_kind.unwrap().0, "browser_asset");
    }
    let health = |c: &str| -> HealthState {
        fx.svc()
            .health(0)
            .into_iter()
            .find(|h| h.component == c)
            .unwrap()
            .health
    };
    assert!(
        matches!(health("browser.asset.chrome"), HealthState::Degraded),
        "present, not yet verified"
    );
    assert!(matches!(
        health("browser.asset.edge"),
        HealthState::Unavailable
    ));
    assert!(matches!(
        health("browser.asset.chromium"),
        HealthState::Unavailable
    ));
    // A browser speaking another protocol fails its handshake: the session fails visibly and
    // its tree and profile are released.
    *fx.platform.protocol.lock().unwrap() = "1.2".into();
    let op = fx.accepted(
        &user,
        "browser.sessions",
        json!({"action": {"kind": "Create", "scope": workspace(), "browserAssetId": "chrome"}}),
    );
    assert!(matches!(
        fx.completion(&user, &op),
        OperationCompletion::Failed { .. }
    ));
    assert!(matches!(
        health("browser.asset.chrome"),
        HealthState::Failed
    ));
    let failed = read(&**fx.app().store(), |s| {
        s.browser()
            .browser_sessions(&BrowserFilter::Scope(None), None, 10)
    });
    assert_eq!(failed[0].phase, BrowserPhase::Failed);
    assert!(failed[0].detail.as_ref().unwrap().contains("protocol"));
    assert!(!fx.profile_root(&failed[0].id).exists());
    *fx.platform.protocol.lock().unwrap() = "1.3".into();
    fx.create(&user, workspace());
    assert!(matches!(
        health("browser.asset.chrome"),
        HealthState::Healthy
    ));
    // With no browser present at all, browser automation reports itself unavailable.
    fx.platform
        .assets
        .lock()
        .unwrap()
        .retain(|a| a.id != "chrome");
    fx.svc().refresh_assets();
    assert!(matches!(
        health("browser.automation"),
        HealthState::Unavailable
    ));
}

#[test]
fn shutdown_closes_sessions_and_boot_recovery_settles_an_earlier_boots_sessions() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let closed = fx.create(&user, workspace());
    let identity = fx.identity(&closed);
    let root = fx.profile_root(&closed);
    // An orderly shutdown closes the session through the process owner.
    let fx = fx.restart(true);
    let s = fx.session(&closed);
    assert_eq!(s.phase, BrowserPhase::Closed, "{s:?}");
    assert!(s.detail.unwrap().contains("shutdown"));
    assert_eq!(NativeProcessHost.probe(&identity), Probe::Gone);
    assert!(!root.exists());

    // A boot that ended without shutdown: process recovery reaps its tree, then its session
    // settles and its profile (and an orphan root nobody owns) are removed.
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let identity = fx.identity(&session);
    let root = fx.profile_root(&session);
    let orphan = fx
        .h()
        .root
        .join("browser")
        .join("sessions")
        .join("orphan-root");
    std::fs::create_dir_all(orphan.join("profile")).unwrap();
    let fx = fx.restart(false);
    let s = fx.session(&session);
    assert_eq!(s.phase, BrowserPhase::Failed, "{s:?}");
    assert!(s.detail.unwrap().contains("restart"));
    assert_eq!(NativeProcessHost.probe(&identity), Probe::Gone);
    assert!(!root.exists());
    assert!(!orphan.exists());
    let report = fx.app().report().browser.clone().unwrap();
    assert_eq!(report.settled, 1);
    assert!(report.profiles_removed >= 1);
}

/// Boots a runtime with Git composed and the default 30 s shutdown bound.
fn git_fx() -> (Fx, Arc<devworks_core::services::git::GitService>) {
    let h = Harness::new();
    let platform = Arc::new(FakePlatform::new(&h.root));
    let rt = runtime();
    let store = h.open();
    seed_project(&h);
    let user = principal(&*store, "user-a", PrincipalKind::Human);
    let real: Arc<dyn BrowserPlatform> = platform.clone();
    let mut composition = composition(
        &h,
        store.clone(),
        real,
        config(),
        platform.unverified_stop.clone(),
    );
    composition.git = Some(source_fixture(&h, &*store, user.principal()));
    composition.config.shutdown_bound = Duration::from_secs(30);
    let app = rt.block_on(boot(composition)).unwrap();
    let git = app.git().expect("git is composed").clone();
    let fx = Fx {
        h: Some(h),
        rt,
        app: Some(app),
        platform,
        user,
        keys: Default::default(),
    };
    (fx, git)
}

/// The Git reconcile loop is stopped before the boot tasks are joined, so shutdown with Git
/// composed does not wait out the shutdown bound for it.
#[test]
fn shutdown_with_git_composed_joins_the_git_loop_without_waiting_out_the_bound() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let (mut fx, git) = git_fx();
    fx.drive(100);
    assert!(!git.is_stopped());
    let started = Instant::now();
    fx.shutdown();
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(5),
        "shutdown took {took:?} with a 30 s bound"
    );
    assert!(git.is_stopped());
}

/// Dropping the runtime without `shutdown` ends the Git loop too.
#[test]
fn dropping_the_runtime_with_git_composed_stops_the_git_loop() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let (mut fx, git) = git_fx();
    fx.drive(100);
    assert!(!git.is_stopped());
    drop(fx.app.take());
    assert!(git.is_stopped());
}

/// Closing the browser sessions at shutdown waits only after the run-control hold committed and
/// only within the one shutdown bound: a close that does not return is abandoned at the
/// deadline, so it can neither delay the hold nor keep shutdown from finishing.
#[test]
fn a_blocked_browser_close_never_delays_the_hold_and_is_abandoned_at_the_shutdown_bound() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let h = Harness::new();
    let platform = Arc::new(FakePlatform::new(&h.root));
    let rt = runtime();
    let store = h.open();
    seed_project(&h);
    let user = principal(&*store, "user-a", PrincipalKind::Human);
    let real: Arc<dyn BrowserPlatform> = platform.clone();
    let mut composition = composition(&h, store, real, config(), platform.unverified_stop.clone());
    composition.config.shutdown_bound = Duration::from_millis(500);
    let app = rt.block_on(boot(composition)).unwrap();
    let mut fx = Fx {
        h: Some(h),
        rt,
        app: Some(app),
        platform: platform.clone(),
        user: user.clone(),
        keys: Default::default(),
    };
    let _session = fx.create(&user, workspace());
    let gate = Arc::new(Gate::default());
    *platform.removal.lock().unwrap() = Some(gate.clone());
    // Opens the gate however the test ends: the runtime waits for its blocking threads.
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    let _release = Release(gate.clone());

    let app = fx.app.take().unwrap();
    let (stage_at_close, entered) = std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            let until = Instant::now() + Duration::from_secs(30);
            while !gate.entered.load(std::sync::atomic::Ordering::SeqCst) && Instant::now() < until
            {
                std::thread::yield_now();
            }
            app.shutdown_stage()
        });
        fx.rt.block_on(within("shutdown", app.shutdown()));
        (
            watcher.join().unwrap(),
            gate.entered.load(std::sync::atomic::Ordering::SeqCst),
        )
    });
    assert!(entered, "shutdown reached the browser close");
    assert!(
        stage_at_close >= ShutdownStage::StoppingWork,
        "the close started only after the hold committed: {stage_at_close:?}"
    );
    assert_eq!(app.shutdown_stage(), ShutdownStage::Done);
    assert!(
        !*gate.open.lock().unwrap(),
        "shutdown returned while the close was still blocked"
    );
    assert!(
        app.processes().owned().is_empty(),
        "the browser tree is stopped"
    );
    gate.release();
    drop(app);
}

#[test]
fn closing_a_session_while_it_starts_never_leaves_a_browser_running() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let create = fx.accepted(
        &user,
        "browser.sessions",
        json!({"action": {"kind": "Create", "scope": workspace(), "browserAssetId": "chrome"}}),
    );
    let sessions = read(&**fx.app().store(), |s| {
        s.browser()
            .browser_sessions(&BrowserFilter::Scope(None), None, 10)
    });
    let session = sessions[0].id.clone();
    let s = fx.session(&session);
    let reply = fx.command(&user, "browser.sessions", &fx.key(), json!({"action": {"kind": "Close", "sessionId": session.as_str(), "expectedRevision": s.revision.to_string()}}));
    let _ = fx.completion(&user, &create);
    if let DomainDecision::Accepted { operation_id } = Fx::decision(&reply) {
        let _ = fx.completion(&user, operation_id);
    }
    let s = fx.session(&session);
    assert!(
        matches!(s.phase, BrowserPhase::Closed | BrowserPhase::Failed),
        "{s:?}"
    );
    if s.process_id.is_some() {
        assert_eq!(NativeProcessHost.probe(&fx.identity(&session)), Probe::Gone);
    }
    assert!(!fx.profile_root(&session).exists());
}

/// Opens a gate however the test ends (the runtime waits for its blocking threads).
struct OpenOnDrop(Arc<Gate>);
impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}
/// Clears the unverified-stop hook however the test ends.
struct VerifyOnDrop(Arc<std::sync::atomic::AtomicBool>);
impl Drop for VerifyOnDrop {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A Close accepted while the session's Create holds it at its launch: the Create passed its
/// `Starting` check and nothing is launched or recorded yet. The Close records its request and
/// the session stays `Starting`; it never settles while the launch may still produce a tree.
/// Returns the session, the Create and Close operations, and the launch gate's guard.
fn close_while_launching(
    fx: &Fx,
    user: &InvocationContext,
) -> (Id, OperationId, OperationId, OpenOnDrop) {
    let gate = Arc::new(Gate::default());
    *fx.platform.launch.lock().unwrap() = Some(gate.clone());
    let guard = OpenOnDrop(gate.clone());
    let create = fx.accepted(
        user,
        "browser.sessions",
        json!({"action": {"kind": "Create", "scope": workspace(), "browserAssetId": "chrome"}}),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !gate.entered.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the Create reached its launch");
        fx.drive(10);
    }
    *fx.platform.launch.lock().unwrap() = None;
    let session = read(&**fx.app().store(), |s| {
        s.browser()
            .browser_sessions(&BrowserFilter::Scope(None), None, 10)
    })[0]
        .id
        .clone();
    let s = fx.session(&session);
    assert_eq!(s.phase, BrowserPhase::Starting, "{s:?}");
    let close = fx.accepted(
        user,
        "browser.sessions",
        json!({"action": {"kind": "Close", "sessionId": session.as_str(), "expectedRevision": s.revision.to_string()}}),
    );
    // The Close's own effect runs meanwhile; it waits for the Create, settling nothing.
    fx.drive(200);
    let s = fx.session(&session);
    assert_eq!(s.phase, BrowserPhase::Starting, "{s:?}");
    assert_eq!(s.close_operation_id.as_ref(), Some(&close), "{s:?}");
    assert_eq!(close_requested_state(fx, &session), "Starting");
    (session, create, close, guard)
}

/// A short stop bound, so an unverified stop gives up quickly.
fn quick_stop() -> BrowserConfig {
    BrowserConfig {
        close_grace: Duration::from_millis(50),
        close_bound: Duration::from_millis(500),
        ..config()
    }
}
fn fx_with(browser: BrowserConfig) -> Fx {
    let h = Harness::new();
    let platform = Arc::new(FakePlatform::new(&h.root));
    let real: Arc<dyn BrowserPlatform> = platform.clone();
    Fx::boot_with(h, platform, real, browser)
}

#[test]
fn a_close_during_launch_stops_the_launched_tree_before_it_settles_closed() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let (session, create, close, gate) = close_while_launching(&fx, &user);
    gate.0.release();
    assert_eq!(
        failed_kind(fx.completion(&user, &create)),
        "browser_session_state"
    );
    fx.succeeded(&user, &close);
    let s = fx.session(&session);
    assert_eq!(s.phase, BrowserPhase::Closed, "{s:?}");
    // The tree the Create launched is recorded on the session and verified gone.
    assert!(s.process_id.is_some(), "{s:?}");
    assert_eq!(NativeProcessHost.probe(&fx.identity(&session)), Probe::Gone);
    assert!(!fx.profile_root(&session).exists());
    assert!(fx.app().processes().owned().is_empty());
}

#[test]
fn a_close_during_launch_whose_stop_is_unverified_stays_uncertain_with_its_profile() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = fx_with(quick_stop());
    let user = fx.user.clone();
    let (session, create, close, gate) = close_while_launching(&fx, &user);
    let hook = fx.platform.unverified_stop.clone();
    let _verify = VerifyOnDrop(hook.clone());
    hook.store(true, std::sync::atomic::Ordering::SeqCst);
    gate.0.release();
    let _ = fx.completion(&user, &create);
    fx.succeeded(&user, &close);
    let s = fx.session(&session);
    // Never closed over a browser that may still run: the tree stays associated, the
    // request stays recorded, and the profile is kept.
    assert_eq!(s.phase, BrowserPhase::Uncertain, "{s:?}");
    assert!(
        s.detail
            .as_deref()
            .unwrap()
            .contains("could not be verified stopped"),
        "{s:?}"
    );
    assert!(s.process_id.is_some(), "{s:?}");
    assert_eq!(s.close_operation_id.as_ref(), Some(&close));
    assert!(fx.profile_root(&session).exists(), "the profile is kept");
    // Once a stop can be verified, a later Close finishes the job.
    hook.store(false, std::sync::atomic::Ordering::SeqCst);
    let s = fx.close(&user, &session);
    assert_eq!(s.phase, BrowserPhase::Closed, "{s:?}");
    assert_eq!(NativeProcessHost.probe(&fx.identity(&session)), Probe::Gone);
    assert!(!fx.profile_root(&session).exists());
}

#[test]
fn a_close_during_a_launch_that_fails_settles_closed_with_nothing_running() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let (session, create, close, gate) = close_while_launching(&fx, &user);
    fx.platform
        .fail_launch
        .store(true, std::sync::atomic::Ordering::SeqCst);
    gate.0.release();
    let _ = fx.completion(&user, &create);
    fx.succeeded(&user, &close);
    let s = fx.session(&session);
    assert_eq!(s.phase, BrowserPhase::Closed, "{s:?}");
    assert!(s.process_id.is_none(), "{s:?}");
    assert!(!fx.profile_root(&session).exists());
    assert!(fx.app().processes().owned().is_empty());
}

#[test]
fn boot_recovery_finishes_a_close_requested_while_the_browser_was_launching() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let gone = fx.create(&user, workspace());
    let unverified = fx.create(&user, workspace());
    let (gone_root, unverified_root) = (fx.profile_root(&gone), fx.profile_root(&unverified));
    let process = fx.session(&unverified).process_id.unwrap();
    // As if each boot ended after its Close was accepted while the browser was launching: the
    // sessions are still `Starting` with the request, and only the create operations' launch
    // records name their trees. (The create operation stands in for the accepted Close: only
    // the request's presence matters to recovery.) The second tree cannot be verified gone at
    // the next boot (recorded for another platform).
    let other = if cfg!(windows) { "unix" } else { "windows" };
    let fx = fx.restart_with(false, |h| {
        let c = rusqlite::Connection::open(h.root.join("workspace.sqlite")).unwrap();
        for s in [&gone, &unverified] {
            c.execute(
                "UPDATE browser_sessions SET state='Starting', process_id=NULL, close_operation_id=create_operation_id WHERE id=?1",
                [s.as_str()],
            )
            .unwrap();
        }
        c.execute(
            "UPDATE owned_processes SET platform=?2 WHERE id=?1",
            [process.as_str(), other],
        )
        .unwrap();
    });
    // Its tree verified gone, the session closes as requested and its profile is removed.
    let s = fx.session(&gone);
    assert_eq!(s.phase, BrowserPhase::Closed, "{s:?}");
    assert!(!gone_root.exists());
    // Unverified, it stays uncertain with its tree, its request and its profile.
    let s = fx.session(&unverified);
    assert_eq!(s.phase, BrowserPhase::Uncertain, "{s:?}");
    assert_eq!(s.process_id.as_ref(), Some(&process), "{s:?}");
    assert!(s.close_operation_id.is_some(), "{s:?}");
    assert!(
        unverified_root.exists(),
        "the profile of an unverified tree is kept"
    );
}

#[test]
fn actions_run_in_commit_order_and_a_failed_one_does_not_block_the_next() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let s = fx.session(&session);
    let rev = s.revision.to_string();
    let name = node(&snap, &ax, "Name");
    // Three actions admitted back to back, the middle one invalid at execution.
    let ops: Vec<OperationId> = [
        json!({"kind": "Text", "node": name, "text": "a"}),
        json!({"kind": "Click", "node": node(&snap, &ax, "Note"), "button": "Primary"}),
        json!({"kind": "Text", "node": name, "text": "b"}),
    ]
    .into_iter()
    .map(|action| {
        fx.accepted(
            &user,
            "browser.input",
            json!({"sessionId": session.as_str(), "expectedRevision": rev, "action": action}),
        )
    })
    .collect();
    assert!(matches!(
        fx.completion(&user, &ops[0]),
        OperationCompletion::Succeeded { .. }
    ));
    assert!(matches!(
        fx.completion(&user, &ops[1]),
        OperationCompletion::Failed { .. }
    ));
    assert!(matches!(
        fx.completion(&user, &ops[2]),
        OperationCompletion::Succeeded { .. }
    ));
    let b = fx.platform.browser(&session);
    assert_eq!(
        b.lock().unwrap().pages[0]
            .values
            .get(&10)
            .map(String::as_str),
        Some("ab")
    );
    // Admitting an action is not a change of the session: the revision is unchanged.
    assert_eq!(fx.session(&session).revision, s.revision);
}

#[test]
fn an_action_waits_for_its_predecessor_to_settle_before_anything_is_sent() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    // A predecessor that has not run yet (as if its delivery were still queued).
    let (sid, pending) = (session.clone(), OperationId(id("pending-before")));
    let p = pending.clone();
    write(&**fx.app().store(), move |tx| {
        tx.operations().record_intent(NewOperation {
            id: p.clone(),
            root_id: None,
            assignment_id: None,
            capability: "browser.input".into(),
            schema_version: 1,
            args_digest: "0".repeat(64),
            payload: json!({"sessionId": sid.as_str()}),
        })?;
        tx.browser().set_last_action(&sid, &p)
    })
    .unwrap();
    let s = fx.session(&session);
    let op = fx.accepted(&user, "browser.input", json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "action": {"kind": "Text", "node": node(&snap, &ax, "Name"), "text": "z"}}));
    let b = fx.platform.browser(&session);
    fx.drive(600);
    assert_eq!(
        requests(&b, "Input.insertText"),
        0,
        "nothing is sent before the predecessor settled"
    );
    assert!(fx.svc().completion(&user, &op).unwrap().is_none());
    // Once the predecessor settles, the action runs.
    let p = pending.clone();
    write(&**fx.app().store(), move |tx| {
        tx.operations()
            .transition(&p, 1, None, OperationState::Failed, None)
            .map(|_| ())
    })
    .unwrap();
    fx.observation(fx.completion(&user, &op));
    assert_eq!(requests(&b, "Input.insertText"), 1);
}

#[test]
fn out_of_process_frames_are_inspected_driven_and_revisioned_on_their_own_session() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/oopif"));
    let ax = fx.json(&snap.accessibility);
    // The cross-site frame is listed as an out-of-process frame of the target.
    let frame = ax["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["outOfProcess"] == true)
        .unwrap_or_else(|| panic!("no out-of-process frame in {}", ax["frames"]))
        .clone();
    let mut target = target_json(&snap);
    target["frameId"] = frame["frameId"].clone();
    let b = fx.platform.browser(&session);
    let child = b.lock().unwrap().children[0].session.clone();
    // Inspection reads the frame's own document on its own session.
    let inner = fx.inspect(&user, &target);
    let inner_ax = fx.json(&inner.accessibility);
    assert_eq!(inner_ax["frameId"], frame["frameId"]);
    assert!(
        inner_ax["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["name"] == "Inside")
    );
    assert!(
        b.lock()
            .unwrap()
            .log
            .iter()
            .any(|m| m["method"] == "Accessibility.getFullAXTree"
                && m["sessionId"] == child.as_str())
    );
    // A click lands inside the frame (placed through its owner's box); text reaches its field.
    fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Click", "node": node(&inner, &inner_ax, "Inside"), "button": "Primary"}),
    ));
    fx.observation(fx.input(&user, &session, json!({"kind": "Text", "node": node(&inner, &inner_ax, "Partner field"), "text": "cross-site"})));
    {
        let br = b.lock().unwrap();
        assert_eq!(br.children[0].clicked, 1);
        assert_eq!(br.children[0].value, "cross-site");
        // DOM reads went to the frame's session, pointer input to the page's.
        assert!(
            br.log
                .iter()
                .any(|m| m["method"] == "DOM.getContentQuads" && m["sessionId"] == child.as_str())
        );
        assert!(
            br.log
                .iter()
                .any(|m| m["method"] == "DOM.getFrameOwner" && m["sessionId"] == "S1")
        );
        assert!(
            br.log
                .iter()
                .filter(|m| m["method"] == "Input.dispatchMouseEvent")
                .all(|m| m["sessionId"] == "S1")
        );
    }
    let console = match fx.result(
        &user,
        "browser.console",
        json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 50}}),
    ) {
        OperationResult::BrowserConsole { value } => value,
        other => panic!("{other:?}"),
    };
    let text = console
        .items
        .0
        .iter()
        .map(|o| fx.json(&o.evidence).to_string())
        .collect::<String>();
    assert!(
        text.contains("inside clicked"),
        "the frame's notifications are the target's: {text}"
    );
    assert!(!text.contains("limitation"), "{text}");
    // When the frame's session detaches, its frame leaves the target and its references go stale.
    b.lock().unwrap().emit(
        Some("S1"),
        "Target.detachedFromTarget",
        json!({"sessionId": child}),
    );
    fx.svc().tick();
    let fault = fx.fault(&user, "browser.inspect", json!({"target": target}));
    assert_eq!(fault.code, DomainErrorCode::SourceChanged);
}

/// The page and its out-of-process partner frame, as wire targets.
fn oopif_targets(fx: &Fx, user: &InvocationContext, session: &Id) -> (Value, Value) {
    let snap = fx.snapshot(fx.navigate(user, session, Some("t1"), "https://app.test/oopif"));
    let ax = fx.json(&snap.accessibility);
    let frame = ax["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["outOfProcess"] == true)
        .unwrap()
        .clone();
    let page = target_json(&snap);
    let mut target = page.clone();
    target["frameId"] = frame["frameId"].clone();
    (page, target)
}

fn failed_kind(c: OperationCompletion) -> String {
    match c {
        OperationCompletion::Failed { fault, .. } => fault.target_kind.unwrap().0,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_scroll_reaches_the_requested_frame_or_is_refused_where_it_cannot() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let (page, frame) = oopif_targets(&fx, &user, &session);
    let b = fx.platform.browser(&session);
    // The frame's own document scrolls, not the page around it.
    fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Scroll", "target": frame, "x": 0, "y": 300}),
    ));
    {
        let br = b.lock().unwrap();
        assert_eq!(
            (br.children[0].scrolled, br.pages[0].scrolled),
            (300.0, 0.0)
        );
    }
    // The page scrolls at a point no frame covers.
    fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Scroll", "target": page, "x": 0, "y": 200}),
    ));
    {
        let br = b.lock().unwrap();
        assert_eq!(
            (br.children[0].scrolled, br.pages[0].scrolled),
            (300.0, 200.0)
        );
    }
    // A covered frame and a frame out of view are refused before any wheel event is sent.
    let wheels = || {
        b.lock()
            .unwrap()
            .log
            .iter()
            .filter(|m| m["params"]["type"] == "mouseWheel")
            .count()
    };
    let before = wheels();
    b.lock().unwrap().frame_covered = true;
    let scroll = json!({"kind": "Scroll", "target": frame, "x": 0, "y": 300});
    assert_eq!(
        failed_kind(fx.input(&user, &session, scroll.clone())),
        "browser_frame"
    );
    {
        let mut br = b.lock().unwrap();
        br.frame_covered = false;
        br.frame_hidden = true;
    }
    assert_eq!(
        failed_kind(fx.input(&user, &session, scroll)),
        "browser_frame"
    );
    assert_eq!(wheels(), before);
    assert_eq!(b.lock().unwrap().children[0].scrolled, 300.0);
}

#[test]
fn a_key_reaches_only_the_frame_holding_keyboard_focus() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let (page, frame) = oopif_targets(&fx, &user, &session);
    let b = fx.platform.browser(&session);
    let inner = fx.inspect(&user, &frame);
    let inner_ax = fx.json(&inner.accessibility);
    // Typing into the partner field moves keyboard focus into the frame.
    fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Text", "node": node(&inner, &inner_ax, "Partner field"), "text": "x"}),
    ));
    let before = requests(&b, "Input.dispatchKeyEvent");
    // A key aimed at the page is refused unsent: the page's document no longer holds focus.
    let key =
        |target: &Value| json!({"kind": "Key", "target": target, "key": "Enter", "modifiers": []});
    assert_eq!(
        failed_kind(fx.input(&user, &session, key(&page))),
        "browser_focus"
    );
    assert_eq!(requests(&b, "Input.dispatchKeyEvent"), before);
    // Aimed at the frame holding focus, it is sent.
    fx.observation(fx.input(&user, &session, key(&frame)));
    assert_eq!(requests(&b, "Input.dispatchKeyEvent"), before + 2);
}

#[test]
fn a_long_multibyte_file_name_is_staged_within_the_platform_name_bound() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    // Fifty three-byte characters: 50 characters, but 154 bytes with the extension.
    let title = format!("{}.txt", "報".repeat(50));
    let e = Evidence {
        scope: ArtifactScope::Workspace,
        producer: user.principal().clone(),
        principal_kind: "Human".into(),
        agent_session: None,
        root: None,
        assignment: None,
        title: title.clone(),
        media_type: "text/plain".into(),
        tags: vec![],
        source: json!({"kind": "Fixture"}),
    };
    let file = fx
        .app()
        .artifacts()
        .publish_evidence(&**fx.app().store(), e, b"wide name", |_, _| Ok(()))
        .unwrap()
        .0;
    let op = fx.accepted(
        &user,
        "browser.upload",
        json!({"node": node(&snap, &ax, "Attachment"), "files": [file]}),
    );
    fx.observation(fx.completion(&user, &op));
    let b = fx.platform.browser(&session);
    let staged = PathBuf::from(&b.lock().unwrap().pages[0].files[&12][0]);
    let name = staged.file_name().unwrap().to_str().unwrap().to_owned();
    assert!(name.len() <= 128, "{} bytes: {name}", name.len());
    assert!(name.starts_with("1-報") && name.ends_with(".txt"), "{name}");
    assert_eq!(std::fs::read(&staged).unwrap(), b"wide name");
}

#[test]
fn actions_waiting_on_a_predecessor_hold_no_dispatch_capacity_so_close_is_never_starved() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let h = Harness::new();
    let platform = Arc::new(FakePlatform::new(&h.root));
    let real: Arc<dyn BrowserPlatform> = platform.clone();
    // The predecessor's call is never answered within the test: only Close's stop ends it; and
    // a successor would wait for it (the production order bound) far longer than the test.
    let fx = Fx::boot_with(
        h,
        platform,
        real,
        BrowserConfig {
            call_timeout: Duration::from_secs(60),
            order_bound: Duration::from_secs(120),
            ..config()
        },
    );
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    fx.platform
        .browser(&session)
        .lock()
        .unwrap()
        .hang
        .insert("Input.insertText".into());
    let rev = fx.session(&session).revision.to_string();
    fx.accepted(
        &user,
        "browser.input",
        json!({"sessionId": session.as_str(), "expectedRevision": rev, "action": {"kind": "Text", "node": node(&snap, &ax, "Name"), "text": "a"}}),
    );
    // As many actions queued behind it as the dispatcher has workers.
    let key = json!({"kind": "Key", "target": target_json(&snap), "key": "Tab", "modifiers": []});
    for _ in 0..4 {
        fx.accepted(
            &user,
            "browser.input",
            json!({"sessionId": session.as_str(), "expectedRevision": rev, "action": key}),
        );
    }
    // The predecessor and the successors behind it are claimed: were waiting to hold a worker,
    // every worker would now be taken until the predecessor's call ends.
    let deadline = Instant::now() + Duration::from_secs(20);
    while read(&**fx.app().store(), |s| {
        s.queue().status(EFFECT_CONSUMER).map(Some)
    })
    .is_none_or(|q| q.claimed < 4)
    {
        assert!(Instant::now() < deadline, "the actions were never claimed");
        fx.drive(10);
    }
    // Close is dispatched at once and ends the running action (its completion is awaited far
    // shorter than that action's call bound).
    let closed = fx.close(&user, &session);
    assert_eq!(closed.phase, BrowserPhase::Closed);
}

#[test]
fn an_action_never_overtakes_a_predecessor_that_outlasts_the_order_bound() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let h = Harness::new();
    let platform = Arc::new(FakePlatform::new(&h.root));
    let real: Arc<dyn BrowserPlatform> = platform.clone();
    // The predecessor (text input that is never answered) runs far longer than the order bound.
    let fx = Fx::boot_with(
        h,
        platform,
        real,
        BrowserConfig {
            call_timeout: Duration::from_secs(3),
            order_bound: Duration::from_millis(200),
            ..config()
        },
    );
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let rev = fx.session(&session).revision.to_string();
    fx.platform
        .browser(&session)
        .lock()
        .unwrap()
        .hang
        .insert("Input.insertText".into());
    let slow = fx.accepted(
        &user,
        "browser.input",
        json!({"sessionId": session.as_str(), "expectedRevision": rev, "action": {"kind": "Text", "node": node(&snap, &ax, "Name"), "text": "a"}}),
    );
    let key = json!({"kind": "Key", "target": target_json(&snap), "key": "Tab", "modifiers": []});
    fx.accepted(
        &user,
        "browser.input",
        json!({"sessionId": session.as_str(), "expectedRevision": rev, "action": key}),
    );
    // Past the order bound, with the text input still running, the key has not been sent.
    fx.drive(1500);
    let b = fx.platform.browser(&session);
    assert_eq!(
        read(&**fx.app().store(), move |s| s
            .operations()
            .operation(&slow))
        .map(|o| o.state),
        Some(OperationState::Dispatching),
        "the predecessor is still running"
    );
    assert_eq!(
        requests(&b, "Input.dispatchKeyEvent"),
        0,
        "the successor overtook its running predecessor"
    );
}

#[test]
fn a_closed_project_starts_no_browser_session() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let c = rusqlite::Connection::open(fx.h().root.join("workspace.sqlite")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch(
        "INSERT INTO projects(id,title,lifecycle,revision,scope_revision,created_at) VALUES('project-c','C','Archived',1,1,0),('project-d','D','Deleted',1,1,0);",
    )
    .unwrap();
    drop(c);
    // An archived project starts no work; a deleted one is gone.
    for (p, code, kind) in [
        ("project-c", DomainErrorCode::Conflict, "archived_project"),
        ("project-d", DomainErrorCode::NotFound, "project"),
    ] {
        let fault = fx.refused(
            &user,
            "browser.sessions",
            json!({"action": {"kind": "Create", "scope": {"kind": "Project", "projectId": p}, "browserAssetId": "chrome"}}),
        );
        assert_eq!(
            (fault.code, fault.target_kind.unwrap().0),
            (code, kind.to_owned()),
            "{p}"
        );
    }
}

#[test]
fn a_close_is_never_held_by_a_pause_of_the_sessions_root() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let run = |op: &str, command: Value| {
        let frame = json!({"protocol": "1.0-draft.1", "requestKey": fx.key(), "command": command});
        let reply = fx
            .app()
            .command(&user, op, frame.to_string().as_bytes())
            .expect("addressable");
        assert!(
            matches!(Fx::decision(&reply), DomainDecision::Applied { .. }),
            "{op}: {reply:?}"
        );
    };
    let root = devworks_core::protocol::RootId(id("root-a"));
    let control = |r: &devworks_core::protocol::RootId| {
        let r = r.clone();
        read(&**fx.app().store(), move |s| s.runs().root_control(&r))
            .map_or(1, |c| c.revision)
            .to_string()
    };
    run(
        "runs.start",
        json!({"kind": "runs.start", "rootId": "root-a", "expectedWorkRevision": "1", "expectedControlRevision": control(&root)}),
    );
    // A session created within the running root.
    let in_root =
        user.clone()
            .with_lineage(Some(ProjectId(id("project-a"))), Some(root.clone()), None);
    let session = fx.create(
        &in_root,
        json!({"kind": "Project", "projectId": "project-a"}),
    );
    let identity = fx.identity(&session);
    run(
        "runs.pause",
        json!({"kind": "runs.pause", "rootId": "root-a", "expectedControlRevision": control(&root)}),
    );
    // A person outside the root closes it: cleanup is never held by the root's pause.
    let closed = fx.close(&user, &session);
    assert_eq!(closed.phase, BrowserPhase::Closed);
    assert_eq!(NativeProcessHost.probe(&identity), Probe::Gone);
}

#[test]
fn a_notice_page_never_skips_an_observation_published_meanwhile() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let console = |after: i64| {
        match fx.result(
        &user,
        "browser.console",
        json!({"sessionId": session.as_str(), "afterSequence": after.to_string(), "page": {"limit": 50}}),
    ) {
        OperationResult::BrowserConsole { value } => value
            .items
            .0
            .iter()
            .map(|o| o.sequence.get())
            .collect::<Vec<i64>>(),
        other => panic!("{other:?}"),
    }
    };
    let last = console(0).into_iter().max().unwrap_or(0);
    let b = fx.platform.browser(&session);
    {
        // A notification for query A, whose reads stall once its snapshot is open and it holds
        // the session's connection.
        let mut br = b.lock().unwrap();
        let s = br.pages[0].session.clone();
        br.emit(
            Some(&s),
            "Runtime.consoleAPICalled",
            json!({"type": "log", "args": [{"type": "string", "value": "second"}]}),
        );
        br.stall_reads = true;
    }
    std::thread::scope(|scope| {
        let a = scope.spawn(|| console(last));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !b.lock().unwrap().reads_stalled {
            assert!(Instant::now() < deadline, "query A never read");
            std::thread::sleep(Duration::from_millis(2));
        }
        // Meanwhile another publication of the session's console channel commits.
        let sid = session.clone();
        let e = Evidence {
            scope: ArtifactScope::Workspace,
            producer: user.principal().clone(),
            principal_kind: "Human".into(),
            agent_session: None,
            root: None,
            assignment: None,
            title: "Browser console observation".into(),
            media_type: "application/json".into(),
            tags: vec![],
            source: json!({"kind": "BrowserConsole"}),
        };
        fx.app()
            .artifacts()
            .publish_evidence(&**fx.app().store(), e, b"{}", move |tx, r| {
                let sequence = tx.browser().next_observation_sequence(&sid)?;
                let recorded_at_ms = tx.now_ms();
                tx.browser()
                    .insert_browser_observation(&StoredBrowserObservation {
                        tested_repository: None,
                        session_id: sid.clone(),
                        sequence,
                        channel: BrowserChannel::Console,
                        artifact_id: r.artifact_id.clone(),
                        version_id: r.version_id.clone(),
                        truncated: false,
                        recorded_at_ms,
                    })
            })
            .unwrap();
        b.lock().unwrap().stall_reads = false;
        // A publishes its own notification after the other one: its page holds both, so a
        // continuation after its last sequence never skips the other.
        assert_eq!(a.join().unwrap(), vec![last + 1, last + 2]);
    });
}

#[test]
fn close_ends_an_action_waiting_on_an_unanswered_call_without_waiting_for_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let h = Harness::new();
    let platform = Arc::new(FakePlatform::new(&h.root));
    let real: Arc<dyn BrowserPlatform> = platform.clone();
    // The call is never answered within the test: only Close's stop of the tree ends it.
    let fx = Fx::boot_with(
        h,
        platform,
        real,
        BrowserConfig {
            call_timeout: Duration::from_secs(60),
            ..config()
        },
    );
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let identity = fx.identity(&session);
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let b = fx.platform.browser(&session);
    b.lock().unwrap().hang.insert("Input.insertText".into());
    let rev = fx.session(&session).revision.to_string();
    fx.accepted(
        &user,
        "browser.input",
        json!({"sessionId": session.as_str(), "expectedRevision": rev, "action": {"kind": "Text", "node": node(&snap, &ax, "Name"), "text": "a"}}),
    );
    // The action is inside its unanswered call, holding the connection.
    let deadline = Instant::now() + Duration::from_secs(20);
    while requests(&b, "Input.insertText") == 0 {
        assert!(Instant::now() < deadline, "the action never sent its call");
        fx.drive(10);
    }
    // Close stops the tree through the process owner, which ends the call: it completes within
    // the fixture's completion bound, far shorter than the call's.
    let closed = fx.close(&user, &session);
    assert_eq!(closed.phase, BrowserPhase::Closed);
    assert_eq!(NativeProcessHost.probe(&identity), Probe::Gone);
}

#[test]
fn input_into_a_frame_follows_its_owners_css_transform() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let (_, frame) = oopif_targets(&fx, &user, &session);
    let b = fx.platform.browser(&session);
    // The partner frame's owner is scaled to half its size (around its top-left corner).
    b.lock().unwrap().frame_scale = Some(0.5);
    let inner = fx.inspect(&user, &frame);
    let inner_ax = fx.json(&inner.accessibility);
    fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Click", "node": node(&inner, &inner_ax, "Inside"), "button": "Primary"}),
    ));
    fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Scroll", "target": frame, "x": 0, "y": 120}),
    ));
    let br = b.lock().unwrap();
    assert_eq!(
        br.children[0].clicked, 1,
        "the click landed in the scaled frame"
    );
    assert_eq!(
        (br.children[0].scrolled, br.pages[0].scrolled),
        (120.0, 0.0)
    );
}

#[test]
fn a_session_never_holds_more_targets_than_it_can_record() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let b = fx.platform.browser(&session);
    // The page opens more popups than a session records.
    {
        let mut br = b.lock().unwrap();
        let opener = br.pages[0].target.clone();
        for _ in 0..MAX_TARGETS + 6 {
            br.open_page("https://app.test/popup", Some(&opener));
        }
    }
    fx.svc().tick();
    let s = fx.session(&session);
    assert_eq!(
        s.targets.len(),
        MAX_TARGETS,
        "the recorded targets are the live ones"
    );
    assert_eq!(
        requests(&b, "Target.closeTarget"),
        7,
        "each excess popup is closed"
    );
    let console = match fx.result(
        &user,
        "browser.console",
        json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 50}}),
    ) {
        OperationResult::BrowserConsole { value } => value,
        other => panic!("{other:?}"),
    };
    let text = console
        .items
        .0
        .iter()
        .map(|o| fx.json(&o.evidence).to_string())
        .collect::<String>();
    assert!(text.contains("targetLimit"), "{text}");
    // A navigation that would open another target is refused before anything is sent.
    let created = requests(&b, "Target.createTarget");
    match fx.navigate(&user, &session, None, "https://app.test/form") {
        OperationCompletion::Failed { fault, .. } => {
            assert_eq!(fault.target_kind.unwrap().0, "browser_target_limit")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(requests(&b, "Target.createTarget"), created);
}

#[test]
fn a_page_with_more_frames_than_registered_is_snapshotted_with_its_list_cut() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let b = fx.platform.browser(&session);
    let extra = 300;
    {
        let mut br = b.lock().unwrap();
        let (s, main) = (br.pages[0].session.clone(), br.pages[0].frame.clone());
        for k in 0..extra {
            br.emit(
                Some(&s),
                "Page.frameAttached",
                json!({"frameId": format!("F{k}"), "parentFrameId": main}),
            );
        }
    }
    fx.svc().tick();
    // Every attachment changed the document; the cut list is said to be cut.
    let mut target = target_json(&snap);
    let revision = target["documentRevision"]
        .as_str()
        .unwrap()
        .parse::<i64>()
        .unwrap()
        + extra;
    target["documentRevision"] = json!(revision.to_string());
    let page = fx.inspect(&user, &target);
    let ax = fx.json(&page.accessibility);
    assert_eq!(ax["frames"].as_array().unwrap().len(), MAX_FRAMES);
    assert_eq!(ax["framesTruncated"], true);
}

/// Every query an input awaits before it sends anything (a focus query, a hit test) may let the
/// page navigate meanwhile: the reference is checked again after it, and nothing is sent into
/// the replacement document.
#[test]
fn input_is_never_sent_into_a_document_that_replaced_its_target_during_a_query() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let b = fx.platform.browser(&session);
    let replace_on = |method: &str| {
        b.lock().unwrap().navigate_on = Some((method.into(), "https://app.test/form".into()));
    };

    // The page navigates while the key's focus query is answered (its new document holds
    // focus): the stale target is refused and no key is sent.
    let keys = requests(&b, "Input.dispatchKeyEvent");
    replace_on("Accessibility.getFullAXTree");
    let key = json!({"kind": "Key", "target": target_json(&snap), "key": "Enter", "modifiers": []});
    assert_eq!(
        failed_kind(fx.input(&user, &session, key)),
        "browser_document"
    );
    assert!(
        b.lock().unwrap().navigate_on.is_none(),
        "the page navigated"
    );
    assert_eq!(requests(&b, "Input.dispatchKeyEvent"), keys);

    // The page navigates while a click reads its node's box: nothing is scrolled or clicked
    // in the new document, and the stale node is refused.
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let (scrolls, mice) = (
        requests(&b, "DOM.scrollIntoViewIfNeeded"),
        requests(&b, "Input.dispatchMouseEvent"),
    );
    replace_on("DOM.getContentQuads");
    let click = json!({"kind": "Click", "node": node(&snap, &ax, "Submit"), "button": "Primary"});
    assert_eq!(
        failed_kind(fx.input(&user, &session, click)),
        "browser_document"
    );
    assert_eq!(requests(&b, "DOM.scrollIntoViewIfNeeded"), scrolls);
    assert_eq!(requests(&b, "Input.dispatchMouseEvent"), mice);

    // The page navigates while a click's hit test is answered (the new document has the same
    // layout, so the point still "reaches" a node): no mouse event is sent. The node was
    // already scrolled into view, so the outcome is unknown rather than "nothing happened".
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let mice = requests(&b, "Input.dispatchMouseEvent");
    replace_on("DOM.getNodeForLocation");
    let click = json!({"kind": "Click", "node": node(&snap, &ax, "Submit"), "button": "Primary"});
    assert!(
        matches!(
            fx.input(&user, &session, click),
            OperationCompletion::OutcomeUnknown { .. }
        ),
        "a click after its scroll is an unknown outcome"
    );
    assert!(
        b.lock().unwrap().navigate_on.is_none(),
        "the page navigated"
    );
    assert_eq!(requests(&b, "Input.dispatchMouseEvent"), mice);
}

#[test]
fn navigation_during_pointer_move_stops_the_click_before_press_and_release() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let b = fx.platform.browser(&session);
    let before = b.lock().unwrap().log.len();
    b.lock().unwrap().navigate_on = Some((
        "Input.dispatchMouseEvent".into(),
        "https://app.test/form".into(),
    ));
    let click = json!({"kind": "Click", "node": node(&snap, &ax, "Submit"), "button": "Primary"});
    assert!(matches!(
        fx.input(&user, &session, click),
        OperationCompletion::OutcomeUnknown { .. }
    ));
    let browser = b.lock().unwrap();
    assert!(browser.navigate_on.is_none(), "the pointer move navigated");
    let events: Vec<_> = browser.log[before..]
        .iter()
        .filter(|m| m["method"] == "Input.dispatchMouseEvent")
        .map(|m| m["params"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        events,
        ["mouseMoved"],
        "no click in the replacement document"
    );
}

#[test]
fn a_click_is_never_sent_to_whatever_covers_its_node() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let b = fx.platform.browser(&session);
    // A modal covers the button: the click is refused and no mouse event is sent.
    b.lock().unwrap().cover = Some(11);
    let mice = requests(&b, "Input.dispatchMouseEvent");
    let click = |n: Value| json!({"kind": "Click", "node": n, "button": "Primary"});
    assert_eq!(
        failed_kind(fx.input(&user, &session, click(node(&snap, &ax, "Submit")))),
        "browser_node"
    );
    assert_eq!(requests(&b, "Input.dispatchMouseEvent"), mice);
    // An element of the embedding page covering a frame's owner takes nothing either.
    let (_, frame) = oopif_targets(&fx, &user, &session);
    let inner = fx.inspect(&user, &frame);
    let inner_ax = fx.json(&inner.accessibility);
    b.lock().unwrap().frame_covered = true;
    let mice = requests(&b, "Input.dispatchMouseEvent");
    assert_eq!(
        failed_kind(fx.input(&user, &session, click(node(&inner, &inner_ax, "Inside")))),
        "browser_node"
    );
    assert_eq!(requests(&b, "Input.dispatchMouseEvent"), mice);
    assert_eq!(b.lock().unwrap().children[0].clicked, 0);
    // Uncovered, the click reaches the node in the frame.
    b.lock().unwrap().frame_covered = false;
    fx.observation(fx.input(&user, &session, click(node(&inner, &inner_ax, "Inside"))));
    assert_eq!(b.lock().unwrap().children[0].clicked, 1);
}

#[test]
fn text_is_never_typed_when_its_node_does_not_keep_focus() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let b = fx.platform.browser(&session);
    b.lock().unwrap().steal_focus = true;
    assert_eq!(
        failed_kind(fx.input(
            &user,
            &session,
            json!({"kind": "Text", "node": node(&snap, &ax, "Name"), "text": "Ada"})
        )),
        "browser_focus"
    );
    assert_eq!(requests(&b, "Input.insertText"), 0, "nothing was typed");
}

#[test]
fn an_older_target_list_is_never_recorded_over_a_newer_one() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    // Two lists captured in order (a popup opened, then closed) reach the store out of order.
    let newer = vec!["t1".to_owned()];
    let older = vec!["t1".to_owned(), "t2".to_owned()];
    fx.svc().record_targets(&session, 1_001, newer.clone());
    fx.svc().record_targets(&session, 1_000, older);
    assert_eq!(
        fx.session(&session).targets,
        newer,
        "the closed tab never comes back"
    );
}

#[test]
fn a_click_is_hit_tested_the_way_pointer_input_is_routed() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let b = fx.platform.browser(&session);
    let click =
        || json!({"kind": "Click", "node": node(&snap, &ax, "Submit"), "button": "Primary"});
    // A node with `pointer-events: none` would pass the click to what lies beneath: refused.
    b.lock().unwrap().pointer_none = Some(11);
    let mice = requests(&b, "Input.dispatchMouseEvent");
    assert_eq!(
        failed_kind(fx.input(&user, &session, click())),
        "browser_node"
    );
    assert_eq!(requests(&b, "Input.dispatchMouseEvent"), mice);
    // An overlay with `pointer-events: none` does not stop a click it does not take.
    {
        let mut br = b.lock().unwrap();
        br.pointer_none = None;
        br.cover = Some(11);
        br.cover_inert = true;
    }
    fx.observation(fx.input(&user, &session, click()));
    let console = match fx.result(
        &user,
        "browser.console",
        json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 50}}),
    ) {
        OperationResult::BrowserConsole { value } => value,
        other => panic!("{other:?}"),
    };
    let text = console
        .items
        .0
        .iter()
        .map(|o| fx.json(&o.evidence).to_string())
        .collect::<String>();
    assert!(
        text.contains("submitted"),
        "the click reached the button: {text}"
    );
}

#[test]
fn local_documents_are_never_loaded_or_read_through_an_owned_browser() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let b = fx.platform.browser(&session);
    let failed = |rid: &str| {
        b.lock().unwrap().log.iter().any(|m| {
            m["method"] == "Fetch.failRequest"
                && m["params"]["requestId"] == rid
                && m["params"]["errorReason"] == "BlockedByClient"
        })
    };
    // A page's local subresource or frame request is refused as blocked by the client.
    {
        let mut br = b.lock().unwrap();
        let (s, f) = (br.pages[0].session.clone(), br.pages[0].frame.clone());
        for (rid, kind, url) in [
            ("sub-1", "Image", "file:///etc/passwd"),
            ("frame-1", "Document", "file:///etc/hosts"),
            (
                "fs-1",
                "Document",
                "filesystem:https://app.test/temporary/x",
            ),
        ] {
            br.emit(
                Some(&s),
                "Fetch.requestPaused",
                json!({"requestId": rid, "request": {"url": url, "method": "GET"}, "frameId": f, "resourceType": kind}),
            );
        }
    }
    fx.svc().tick();
    for rid in ["sub-1", "frame-1", "fs-1"] {
        assert!(failed(rid), "{rid} was not refused");
    }
    // A local document that commits anyway (in any frame) is never read and the page is torn
    // down: here the teardown navigation hangs, so the page stays unreadable meanwhile.
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let mut target = target_json(&snap);
    {
        let mut br = b.lock().unwrap();
        br.hang.insert("Page.navigate".into());
        let (s, f) = (br.pages[0].session.clone(), br.pages[0].frame.clone());
        br.emit(
            Some(&s),
            "Page.frameNavigated",
            json!({"frame": {"id": "F-local", "parentId": f, "url": "file:///etc/hosts", "loaderId": "L-local"}}),
        );
    }
    fx.svc().tick();
    assert!(
        b.lock()
            .unwrap()
            .log
            .iter()
            .any(|m| m["method"] == "Page.navigate" && m["params"]["url"] == "about:blank"),
        "the page was torn down"
    );
    let revision = target["documentRevision"]
        .as_str()
        .unwrap()
        .parse::<i64>()
        .unwrap()
        + 1;
    target["documentRevision"] = json!(revision.to_string());
    let fault = fx.fault(&user, "browser.inspect", json!({"target": target}));
    assert_eq!(fault.target_kind.unwrap().0, "browser_local_document");
    let console = match fx.result(
        &user,
        "browser.console",
        json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 50}}),
    ) {
        OperationResult::BrowserConsole { value } => value,
        other => panic!("{other:?}"),
    };
    let text = console
        .items
        .0
        .iter()
        .map(|o| fx.json(&o.evidence).to_string())
        .collect::<String>();
    assert!(text.contains("localDocumentBlocked"), "{text}");
    assert!(
        !text.contains("/etc/hosts"),
        "the local path is not recorded: {text}"
    );
}

#[test]
fn a_browser_whose_main_process_exits_is_settled_while_its_helpers_linger() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let identity = fx.identity(&session);
    let root = fx.profile_root(&session);
    // Only the main process dies; its descendant (a helper) keeps the tree non-empty.
    let pid = identity.pid.to_string();
    let killed = if cfg!(windows) {
        std::process::Command::new("taskkill")
            .args(["/PID", &pid, "/F"])
            .output()
    } else {
        std::process::Command::new("kill")
            .args(["-9", &pid])
            .output()
    };
    assert!(killed.unwrap().status.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    while fx.session(&session).phase == BrowserPhase::Ready {
        assert!(
            Instant::now() < deadline,
            "the main process's exit was not detected"
        );
        fx.svc().tick();
        fx.drive(20);
    }
    let s = fx.session(&session);
    assert_eq!(s.phase, BrowserPhase::Failed, "{s:?}");
    assert!(s.detail.unwrap().contains("exited"));
    // The teardown stopped the lingering helpers too: the whole tree is gone.
    assert_eq!(NativeProcessHost.probe(&identity), Probe::Gone);
    let pid = fx.session(&session).process_id.unwrap();
    let rec = read(&**fx.app().store(), move |s| s.resources().process(&pid)).unwrap();
    assert_eq!(rec.phase, ProcessPhase::Reaped, "{rec:?}");
    assert!(!root.exists());
}

/// Writes the workspace artifact storage limit directly (as a settings save would).
fn set_quota(store: &dyn ApplicationStore, bytes: i64) {
    use devworks_core::settings::catalog::{ARTIFACT_QUOTA_KEY, descriptor};
    use devworks_core::settings::model;
    write(store, move |tx| {
        let current = model::current_layer(tx.configuration(), &ConfigScope::Workspace)?;
        let schema =
            serde_json::to_value(descriptor(ARTIFACT_QUOTA_KEY).unwrap().schema_ref()).unwrap();
        let a = serde_json::from_value(
            json!({"key": ARTIFACT_QUOTA_KEY, "value": {"schema": schema, "value": bytes}}),
        )
        .unwrap();
        let plan = model::plan(&current, &[a]);
        let body = model::body(
            &plan,
            &ActorId(id("host")),
            tx.now_ms(),
            json!({"kind": "test"}),
        );
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            model::NAMESPACE,
            current.revision,
            model::BODY_SCHEMA_VERSION,
            body,
        )?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn a_failed_predecessor_never_releases_actions_queued_behind_an_unknown_one() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    fx.platform
        .browser(&session)
        .lock()
        .unwrap()
        .hang
        .insert("Input.insertText".into());
    let rev = fx.session(&session).revision.to_string();
    let key = json!({"kind": "Key", "target": target_json(&snap), "key": "Tab", "modifiers": []});
    // A (its outcome will be unknown), then B and C queued behind it.
    let ops: Vec<OperationId> = [
        json!({"kind": "Text", "node": node(&snap, &ax, "Name"), "text": "a"}),
        key.clone(),
        key,
    ]
    .into_iter()
    .map(|action| {
        fx.accepted(
            &user,
            "browser.input",
            json!({"sessionId": session.as_str(), "expectedRevision": rev, "action": action}),
        )
    })
    .collect();
    assert!(matches!(
        fx.completion(&user, &ops[0]),
        OperationCompletion::OutcomeUnknown { .. }
    ));
    assert!(matches!(
        fx.completion(&user, &ops[1]),
        OperationCompletion::Failed { .. }
    ));
    match fx.completion(&user, &ops[2]) {
        OperationCompletion::Failed { fault, .. } => {
            assert_eq!(fault.target_kind.unwrap().0, "browser_action_order");
        }
        other => panic!("C ran behind an unreconciled action: {other:?}"),
    }
    let b = fx.platform.browser(&session);
    assert_eq!(
        requests(&b, "Input.dispatchKeyEvent"),
        0,
        "nothing after the unknown action"
    );
}

#[test]
fn a_reference_into_another_session_is_refused_even_when_its_ids_collide() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let a = fx.create(&user, workspace());
    let b = fx.create(&user, workspace());
    // Both sessions start at t1 with the same frame, document and snapshot revisions.
    let snap_a = fx.snapshot(fx.navigate(&user, &a, Some("t1"), "https://app.test/form"));
    let ax_a = fx.json(&snap_a.accessibility);
    let snap_b = fx.snapshot(fx.navigate(&user, &b, Some("t1"), "https://app.test/form"));
    fx.json(&snap_b.accessibility);
    assert_eq!(
        snap_a.target.document_revision,
        snap_b.target.document_revision
    );
    let rev = fx.session(&b).revision.to_string();
    for action in [
        json!({"kind": "Click", "node": node(&snap_a, &ax_a, "Submit"), "button": "Primary"}),
        json!({"kind": "Key", "target": target_json(&snap_a), "key": "Tab", "modifiers": []}),
    ] {
        let fault = fx.refused(
            &user,
            "browser.input",
            json!({"sessionId": b.as_str(), "expectedRevision": rev, "action": action}),
        );
        assert_eq!(
            (fault.code, fault.target_kind.unwrap().0),
            (
                DomainErrorCode::InvalidArgument,
                "browser_target_session".to_owned()
            )
        );
    }
    let fb = fx.platform.browser(&b);
    assert_eq!(
        requests(&fb, "Input.dispatchMouseEvent") + requests(&fb, "Input.dispatchKeyEvent"),
        0
    );
}

#[test]
fn concurrent_evidence_publications_cannot_exceed_the_storage_limit() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let store = fx.app().store().clone();
    set_quota(&*store, 100);
    // Hold the write owner so both publications pass their read-only check first.
    let (release, held) = std::sync::mpsc::channel::<()>();
    let (started, blocking) = std::sync::mpsc::channel::<()>();
    let st = store.clone();
    let blocker = std::thread::spawn(move || {
        run_write(&*st, WriteOptions::default(), move |_| {
            started.send(()).unwrap();
            held.recv().unwrap();
            Ok(())
        })
        .unwrap();
    });
    blocking.recv().unwrap();
    let publish = |byte: u8| {
        let (artifacts, store, owner) = (
            fx.app().artifacts().clone(),
            store.clone(),
            user.principal().clone(),
        );
        std::thread::spawn(move || {
            let e = Evidence {
                scope: ArtifactScope::Workspace,
                producer: owner,
                principal_kind: "Human".into(),
                agent_session: None,
                root: None,
                assignment: None,
                title: "evidence".into(),
                media_type: "application/octet-stream".into(),
                tags: vec![],
                source: json!({"kind": "Fixture"}),
            };
            artifacts
                .publish_evidence(&*store, e, &[byte; 60], |_, _| Ok(()))
                .map(|_| ())
        })
    };
    let (one, two) = (publish(1), publish(2));
    std::thread::sleep(Duration::from_millis(400));
    release.send(()).unwrap();
    blocker.join().unwrap();
    let results = [one.join().unwrap(), two.join().unwrap()];
    let refused = results
        .iter()
        .filter(|r| {
            matches!(
                r,
                Err(devworks_core::services::artifacts::evidence::EvidenceFailure::Refused(_))
            )
        })
        .count();
    assert_eq!(
        (results.iter().filter(|r| r.is_ok()).count(), refused),
        (1, 1),
        "{results:?}"
    );
    let used = read(&*store, |s| s.artifacts().workspace_usage());
    assert!(used <= 100, "workspace usage {used} exceeds the limit");
}

#[test]
fn a_failure_after_the_navigation_was_sent_is_an_unknown_outcome() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let b = fx.platform.browser(&session);
    b.lock()
        .unwrap()
        .hang
        .insert("Accessibility.getFullAXTree".into());
    let c = fx.navigate(&user, &session, Some("t1"), "https://app.test/form");
    assert!(
        matches!(c, OperationCompletion::OutcomeUnknown { .. }),
        "{c:?}"
    );
    assert_eq!(requests(&b, "Page.navigate"), 1, "the navigation was sent");
    assert!(
        fx.session(&session).uncertain_operation_id.is_some(),
        "the session holds further actions"
    );
}

#[test]
fn notifications_stay_pending_when_their_publication_fails() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let b = fx.platform.browser(&session);
    b.lock().unwrap().emit(
        Some("S1"),
        "Runtime.consoleAPICalled",
        json!({"type": "log", "args": [{"type": "string", "value": "kept notice"}]}),
    );
    set_quota(&**fx.app().store(), 1);
    let query = json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 50}});
    assert_eq!(
        fx.fault(&user, "browser.console", query.clone()).code,
        DomainErrorCode::CapacityWait
    );
    set_quota(&**fx.app().store(), 0);
    match fx.result(&user, "browser.console", query) {
        OperationResult::BrowserConsole { value } => {
            let text = value
                .items
                .0
                .iter()
                .map(|o| fx.json(&o.evidence).to_string())
                .collect::<String>();
            assert!(text.contains("kept notice"), "{text}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_largest_notification_page_stays_within_the_repository_bound() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    for op in ["browser.console", "browser.network"] {
        match fx.query(
            &user,
            op,
            json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 500}}),
        ) {
            DomainQueryReply::Result { .. } => {}
            other => panic!("{op} with a page of 500: {other:?}"),
        }
    }
}

#[test]
fn a_queued_action_of_a_revoked_owner_is_never_performed() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    fx.opt_in_agent_browser_tools();
    let user = fx.user.clone();
    let agent = fx.agent("agent-v", AgentRole::Worker, None);
    let session = fx.create(&agent, workspace());
    let snap = fx.snapshot(fx.navigate(&agent, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    // Hold the action behind a predecessor that has not run yet.
    let (sid, pending) = (session.clone(), OperationId(id("pending-revoked")));
    let p = pending.clone();
    write(&**fx.app().store(), move |tx| {
        tx.operations().record_intent(NewOperation {
            id: p.clone(),
            root_id: None,
            assignment_id: None,
            capability: "browser.input".into(),
            schema_version: 1,
            args_digest: "0".repeat(64),
            payload: json!({"sessionId": sid.as_str()}),
        })?;
        tx.browser().set_last_action(&sid, &p)
    })
    .unwrap();
    let s = fx.session(&session);
    let op = fx.accepted(&agent, "browser.input", json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "action": {"kind": "Text", "node": node(&snap, &ax, "Name"), "text": "after revocation"}}));
    // The person revokes the agent while its action waits.
    let u = user.clone();
    write(&**fx.app().store(), move |tx| {
        let actor = ActorId(id("agent-v"));
        let revision = tx.authority().principal(&actor)?.unwrap().revision;
        devworks_core::authority::service::revoke_principal(tx, &u, &actor, revision)
            .map(|_| ())
            .map_err(|e| StorageError::InvalidRecord(format!("{e:?}")))
    })
    .unwrap();
    let p = pending.clone();
    write(&**fx.app().store(), move |tx| {
        tx.operations()
            .transition(&p, 1, None, OperationState::Failed, None)
            .map(|_| ())
    })
    .unwrap();
    match fx.completion(&user, &op) {
        OperationCompletion::Failed { fault, .. } => {
            assert_eq!(
                (fault.code, fault.target_kind.unwrap().0),
                (
                    DomainErrorCode::PermissionDenied,
                    "browser_session_owner".to_owned()
                )
            );
        }
        other => panic!("{other:?}"),
    }
    let b = fx.platform.browser(&session);
    assert_eq!(
        requests(&b, "Input.insertText"),
        0,
        "nothing was sent for the revoked owner"
    );
}

#[test]
fn recovery_finds_a_tree_launched_before_its_session_recorded_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let root = fx.profile_root(&session);
    let process = fx.session(&session).process_id.unwrap();
    // As if the boot ended between the launch and recording the tree on the session, and the
    // tree could not be verified gone at the next boot (here: recorded for another platform).
    let other = if cfg!(windows) { "unix" } else { "windows" };
    let fx = fx.restart_with(false, |h| {
        let c = rusqlite::Connection::open(h.root.join("workspace.sqlite")).unwrap();
        c.execute(
            "UPDATE browser_sessions SET state='Starting', process_id=NULL WHERE id=?1",
            [session.as_str()],
        )
        .unwrap();
        c.execute(
            "UPDATE owned_processes SET platform=?2 WHERE id=?1",
            [process.as_str(), other],
        )
        .unwrap();
    });
    let s = fx.session(&session);
    assert_eq!(s.phase, BrowserPhase::Uncertain, "{s:?}");
    assert!(root.exists(), "the profile of an unverified tree is kept");
    assert_eq!(fx.app().report().browser.clone().unwrap().uncertain, 1);
    // The tree recovery found through the create operation is recorded on the session.
    assert_eq!(s.process_id.as_ref(), Some(&process), "{s:?}");

    // Closing it stops that tree through the process owner; its stop cannot be verified, so
    // the session stays uncertain with its profile, never closed over a possible browser.
    let s = fx.close(&user, &session);
    assert_eq!(s.phase, BrowserPhase::Uncertain, "{s:?}");
    assert!(s.detail.unwrap().contains("could not be verified stopped"));
    assert!(root.exists(), "the profile of an unverified tree is kept");
    assert_eq!(close_requested_state(&fx, &session), "Stopping");
}

/// The state the session's Close decision recorded.
fn close_requested_state(fx: &Fx, session: &Id) -> String {
    let id = session.as_str().to_owned();
    read(&**fx.app().store(), move |s| {
        s.history().page(
            &EventFilter::Aggregate {
                kind: "browser_session".into(),
                id,
            },
            0,
            500,
        )
    })
    .events
    .into_iter()
    .rev()
    .find(|e| e.kind == "BrowserSessionCloseRequested")
    .expect("a close was requested")
    .payload["state"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Clears the session's process ID while the application runs: a session whose tree is only
/// named by its create operation's launch record (a failed or lost write).
fn forget_process_id(fx: &Fx, session: &Id) {
    let c = rusqlite::Connection::open(fx.h().root.join("workspace.sqlite")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute(
        "UPDATE browser_sessions SET process_id=NULL WHERE id=?1",
        [session.as_str()],
    )
    .unwrap();
}

#[test]
fn an_uncertain_session_without_a_recorded_tree_is_never_closed_unverified() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let root = fx.profile_root(&session);
    let process = fx.session(&session).process_id.unwrap();
    let other = if cfg!(windows) { "unix" } else { "windows" };
    let fx = fx.restart_with(false, |h| {
        let c = rusqlite::Connection::open(h.root.join("workspace.sqlite")).unwrap();
        c.execute(
            "UPDATE owned_processes SET platform=?2 WHERE id=?1",
            [process.as_str(), other],
        )
        .unwrap();
    });
    assert_eq!(fx.session(&session).phase, BrowserPhase::Uncertain);
    // The session no longer names its tree; only the create operation's launch record does.
    forget_process_id(&fx, &session);
    let s = fx.close(&user, &session);
    assert_eq!(close_requested_state(&fx, &session), "Stopping");
    assert_eq!(s.phase, BrowserPhase::Uncertain, "{s:?}");
    assert!(root.exists(), "the profile of an unverified tree is kept");
    // The Close adopted the tree from the launch record.
    assert!(fx.session(&session).process_id.is_some());
}

/// Browser reads observe the live session and publish evidence before the gateway takes its
/// read snapshot: with a single pooled reader, none of them waits on the reader its own query
/// holds.
#[test]
fn browser_reads_answer_over_a_store_with_a_single_reader() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let h = Harness::new();
    h.readers.store(1, std::sync::atomic::Ordering::SeqCst);
    let platform = Arc::new(FakePlatform::new(&h.root));
    let fx = Fx::boot(h, platform);
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let started = Instant::now();
    let inspected = fx.inspect(&user, &target_json(&snap));
    assert_eq!(target_json(&inspected), target_json(&snap));
    match fx.result(
        &user,
        "browser.screenshot",
        json!({"target": target_json(&snap), "fullPage": false}),
    ) {
        OperationResult::BrowserScreenshot { value } => assert_eq!(value.media_type, "image/png"),
        other => panic!("{other:?}"),
    }
    let b = fx.platform.browser(&session);
    b.lock().unwrap().emit(
        Some("S1"),
        "Runtime.consoleAPICalled",
        json!({"type": "log", "args": [{"type": "string", "value": "one reader"}]}),
    );
    match fx.result(
        &user,
        "browser.console",
        json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 50}}),
    ) {
        OperationResult::BrowserConsole { value } => {
            let text = value
                .items
                .0
                .iter()
                .map(|o| fx.json(&o.evidence).to_string())
                .collect::<String>();
            assert!(text.contains("one reader"), "{text}");
        }
        other => panic!("{other:?}"),
    }
    match fx.result(
        &user,
        "browser.network",
        json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 50}}),
    ) {
        OperationResult::BrowserNetwork { .. } => {}
        other => panic!("{other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "no read waited for a reader: {:?}",
        started.elapsed()
    );
}

#[test]
fn navigation_without_a_target_registers_a_new_one_and_artifact_pages_are_staged() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let before = fx.session(&session).revision;
    let snap = fx.snapshot(fx.navigate(&user, &session, None, "https://app.test/form"));
    assert_eq!(snap.target.target_id.as_str(), "t2");
    let s = fx.session(&session);
    assert_eq!(s.targets, vec!["t1".to_owned(), "t2".to_owned()]);
    assert!(s.revision > before, "a new target is registered explicitly");
    // An artifact page is materialized under the session root and opened from there.
    let page = Evidence {
        scope: ArtifactScope::Workspace,
        producer: user.principal().clone(),
        principal_kind: "Human".into(),
        agent_session: None,
        root: None,
        assignment: None,
        title: "report.html".into(),
        media_type: "text/html".into(),
        tags: vec![],
        source: json!({"kind": "Fixture"}),
    };
    let r = fx
        .app()
        .artifacts()
        .publish_evidence(
            &**fx.app().store(),
            page,
            b"<title>Report</title>",
            |_, _| Ok(()),
        )
        .unwrap()
        .0;
    let op = fx.accepted(&user, "browser.navigate", json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "targetId": "t1", "location": {"kind": "Artifact", "artifact": r}}));
    fx.snapshot(fx.completion(&user, &op));
    let b = fx.platform.browser(&session);
    // It is served from the session's artifact origin (never a local file URL), fulfilled from
    // its staged copy by request interception.
    let url = b.lock().unwrap().pages[0].url.clone();
    let root = fx.profile_root(&session);
    assert!(
        url.starts_with("https://artifact.devworks.invalid/") && url.ends_with("report.html"),
        "{url}"
    );
    assert!(!url.contains(&*root.to_string_lossy()), "{url}");
    assert_eq!(requests(&b, "Fetch.fulfillRequest"), 1);
    assert_eq!(b.lock().unwrap().pages[0].title, "Report");
    // Console pages: a bounded page with a continuation cursor.
    for _ in 0..3 {
        fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
        fx.result(
            &user,
            "browser.network",
            json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 1}}),
        );
    }
    let first = match fx.result(
        &user,
        "browser.network",
        json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 2}}),
    ) {
        OperationResult::BrowserNetwork { value } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(first.items.0.len(), 2);
    assert!(first.has_more);
    let next = match fx.result(&user, "browser.network", json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 2, "cursor": first.next_cursor.as_ref().unwrap().0}})) {
        OperationResult::BrowserNetwork { value } => value,
        other => panic!("{other:?}"),
    };
    assert!(next.items.0[0].sequence.get() > first.items.0[1].sequence.get());
    // A cursor of another query is refused.
    let fault = fx.fault(&user, "browser.console", json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 2, "cursor": first.next_cursor.unwrap().0}}));
    assert_eq!(fault.code, DomainErrorCode::CursorExpired);
}

// ---------------------------------------------------------------------------------------------
// Opt-in: an installed browser

/// Drives an installed Chromium-family browser end to end through the native platform and
/// transport. Opt-in (`DEVWORKS_BROWSER_TESTS=1`, optionally `DEVWORKS_BROWSER=<path>`); skipped
/// otherwise, which is not a pass. Not run in CI (no browser is installed there).
#[test]
fn an_installed_browser_end_to_end_when_opted_in() {
    if std::env::var("DEVWORKS_BROWSER_TESTS").ok().as_deref() != Some("1") {
        eprintln!("skipped: set DEVWORKS_BROWSER_TESTS=1 to drive an installed browser");
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let h = Harness::new();
    let configured = std::env::var_os("DEVWORKS_BROWSER").map(PathBuf::from);
    let native = Arc::new(NativeBrowserPlatform::new(&h.root, configured));
    let asset = native
        .assets()
        .into_iter()
        .find(|a| a.qualification == Qualification::Present)
        .expect("an installed browser");
    let fake = Arc::new(FakePlatform::new(&h.root));
    let fx = Fx::boot_with(h, fake, native, BrowserConfig::default());
    let user = fx.user.clone();
    // A local fixture page (no external network).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        // One thread per connection: the browser preconnects idle sockets.
        for s in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut s = s;
                let mut buf = [0u8; 4096];
                let n = std::io::Read::read(&mut s, &mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = if req.starts_with("GET /next") {
                    "<!doctype html><title>Next</title><p>next page</p>".to_owned()
                } else if req.starts_with("GET /pointer") {
                    // One button pointer input passes through, one under an overlay it passes.
                    "<!doctype html><title>Pointer</title><div style=\"position:absolute;left:0;top:0;width:400px;height:300px\" onclick=\"console.log('beneath clicked')\"></div><button style=\"position:absolute;left:10px;top:10px;width:100px;height:40px;pointer-events:none\" onclick=\"console.log('inert clicked')\">Inert</button><button style=\"position:absolute;left:10px;top:100px;width:100px;height:40px\" onclick=\"console.log('under clicked')\">Under</button><div style=\"position:absolute;left:0;top:90px;width:200px;height:60px;pointer-events:none\"></div>".to_owned()
                } else if req.starts_with("GET /inner") {
                    "<!doctype html><title>Inner</title><button onclick=\"console.log('inside clicked')\">Inside</button>".to_owned()
                } else if req.starts_with("GET /partner") {
                    // A cross-site frame: localhost and 127.0.0.1 are different sites.
                    format!(
                        "<!doctype html><title>Partner</title><iframe src=\"http://localhost:{port}/inner\" width=300 height=120></iframe>"
                    )
                } else {
                    "<!doctype html><title>Owned fixture</title><label>Name <input id=n></label><button onclick=\"console.log('clicked '+document.getElementById('n').value)\">Go</button><input type=file aria-label=Attachment><a href=/next>Next page</a>".to_owned()
                };
                let _ = std::io::Write::write_all(&mut s, format!("HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes());
            });
        }
    });
    let op = fx.accepted(
        &user,
        "browser.sessions",
        json!({"action": {"kind": "Create", "scope": workspace(), "browserAssetId": asset.id}}),
    );
    let session = match fx.succeeded(&user, &op) {
        OperationResult::BrowserSessions { value } => value.items.0[0].id.clone(),
        other => panic!("{other:?}"),
    };
    let identity = fx.identity(&session);
    let root = fx.profile_root(&session);
    assert_eq!(NativeProcessHost.probe(&identity), Probe::Alive);
    // The pipe transport opens no DevTools port: Chrome publishes DevToolsActivePort only then.
    assert!(
        !root.join("profile").join("DevToolsActivePort").exists(),
        "no DevTools port"
    );
    if cfg!(unix)
        && let Ok(out) = std::process::Command::new("lsof")
            .args([
                "-nP",
                "-a",
                "-iTCP",
                "-sTCP:LISTEN",
                "-p",
                &identity.pid.to_string(),
            ])
            .output()
    {
        let listening = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(
            listening.trim().is_empty(),
            "the browser listens on: {listening}"
        );
    }
    let health = fx.svc().health(0);
    let qualified = health
        .iter()
        .find(|c| c.component == format!("browser.asset.{}", asset.id))
        .unwrap();
    assert!(
        matches!(qualified.health, HealthState::Healthy),
        "{}",
        qualified.message
    );
    eprintln!("qualified: {}", qualified.message);
    let target = fx.session(&session).targets[0].clone();
    let snap = fx.snapshot(fx.navigate(
        &user,
        &session,
        Some(&target),
        &format!("http://127.0.0.1:{port}/"),
    ));
    assert_eq!(snap.title.0, "Owned fixture");
    let ax = fx.json(&snap.accessibility);
    let field = ax["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["role"] == "textbox")
        .expect("textbox")
        .clone();
    let field = json!({"target": target_json(&snap), "nodeId": field["nodeId"], "snapshotRevision": ax["snapshotRevision"].as_i64().unwrap().to_string()});
    fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Text", "node": field, "text": "Ada"}),
    ));
    let go = node(&snap, &ax, "Go");
    fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Click", "node": go, "button": "Primary"}),
    ));
    let console = match fx.result(
        &user,
        "browser.console",
        json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 50}}),
    ) {
        OperationResult::BrowserConsole { value } => value,
        other => panic!("{other:?}"),
    };
    let text = console
        .items
        .0
        .iter()
        .map(|o| fx.json(&o.evidence).to_string())
        .collect::<String>();
    assert!(text.contains("clicked Ada"), "{text}");
    let shot = match fx.result(
        &user,
        "browser.screenshot",
        json!({"target": target_json(&snap), "fullPage": false}),
    ) {
        OperationResult::BrowserScreenshot { value } => value,
        other => panic!("{other:?}"),
    };
    assert!(fx.bytes(&shot).starts_with(b"\x89PNG"));
    // Upload an exact artifact into the file input.
    let file = fx
        .app()
        .artifacts()
        .publish_evidence(
            &**fx.app().store(),
            Evidence {
                scope: ArtifactScope::Workspace,
                producer: user.principal().clone(),
                principal_kind: "Human".into(),
                agent_session: None,
                root: None,
                assignment: None,
                title: "upload.txt".into(),
                media_type: "text/plain".into(),
                tags: vec![],
                source: json!({"kind": "Fixture"}),
            },
            b"owned upload bytes",
            |_, _| Ok(()),
        )
        .unwrap()
        .0;
    let attach = ax["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "Attachment")
        .expect("file input")
        .clone();
    let attach = json!({"target": target_json(&snap), "nodeId": attach["nodeId"], "snapshotRevision": ax["snapshotRevision"].as_i64().unwrap().to_string()});
    let op = fx.accepted(
        &user,
        "browser.upload",
        json!({"node": attach, "files": [file]}),
    );
    fx.observation(fx.completion(&user, &op));
    // A cross-site frame is out of process: inspected and clicked on its own session.
    let partner = fx.snapshot(fx.navigate(
        &user,
        &session,
        Some(&target),
        &format!("http://127.0.0.1:{port}/partner"),
    ));
    // A fresh snapshot of `t`: a document that moved on is probed forward (a read may retry).
    let fresh = |t: &Value| -> (BrowserSnapshot, Value) {
        let mut t = t.clone();
        for _ in 0..60 {
            if let DomainQueryReply::Result {
                result: OperationResult::BrowserInspect { value },
                ..
            } = fx.query(&user, "browser.inspect", json!({"target": t}))
            {
                let ax = fx.json(&value.accessibility);
                return (value, ax);
            }
            let next = t["documentRevision"]
                .as_str()
                .unwrap()
                .parse::<i64>()
                .unwrap()
                + 1;
            t["documentRevision"] = json!(next.to_string());
        }
        panic!("no current document for {t}");
    };
    fx.drive(1500);
    let (partner, pax) = fresh(&target_json(&partner));
    let frame = pax["frames"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["outOfProcess"] == true)
        .unwrap_or_else(|| panic!("no out-of-process frame: {}", pax["frames"]))
        .clone();
    let mut inner_target = target_json(&partner);
    inner_target["frameId"] = frame["frameId"].clone();
    let (inner, inner_ax) = fresh(&inner_target);
    assert!(
        inner_ax["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["name"] == "Inside"),
        "{inner_ax}"
    );
    fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Click", "node": node(&inner, &inner_ax, "Inside"), "button": "Primary"}),
    ));
    let console = match fx.result(
        &user,
        "browser.console",
        json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 50}}),
    ) {
        OperationResult::BrowserConsole { value } => value,
        other => panic!("{other:?}"),
    };
    let text = console
        .items
        .0
        .iter()
        .map(|o| fx.json(&o.evidence).to_string())
        .collect::<String>();
    assert!(text.contains("inside clicked"), "{text}");
    eprintln!("out-of-process frame {} clicked", frame["frameId"]);
    // Pointer routing: a `pointer-events: none` button is refused (the click would land beneath
    // it); a button under a `pointer-events: none` overlay is clicked.
    let pointer = fx.snapshot(fx.navigate(
        &user,
        &session,
        Some(&target),
        &format!("http://127.0.0.1:{port}/pointer"),
    ));
    let pax = fx.json(&pointer.accessibility);
    let click = |name: &str| json!({"kind": "Click", "node": node(&pointer, &pax, name), "button": "Primary"});
    match fx.input(&user, &session, click("Inert")) {
        OperationCompletion::Failed { fault, .. } => {
            assert_eq!(fault.target_kind.unwrap().0, "browser_node")
        }
        other => panic!("a pointer-events: none button was clicked: {other:?}"),
    }
    fx.observation(fx.input(&user, &session, click("Under")));
    let console = match fx.result(
        &user,
        "browser.console",
        json!({"sessionId": session.as_str(), "afterSequence": "0", "page": {"limit": 50}}),
    ) {
        OperationResult::BrowserConsole { value } => value,
        other => panic!("{other:?}"),
    };
    let text = console
        .items
        .0
        .iter()
        .map(|o| fx.json(&o.evidence).to_string())
        .collect::<String>();
    assert!(text.contains("under clicked"), "{text}");
    assert!(
        !text.contains("inert clicked") && !text.contains("beneath clicked"),
        "{text}"
    );
    eprintln!("pointer routing: inert refused, overlay passed");
    // An artifact page is served from the artifact origin: its refresh, link and frame into a
    // local file load nothing, and nothing of the file is ever readable through inspect.
    let secret_dir =
        std::env::temp_dir().join(format!("devworks-local-secret-{}", std::process::id()));
    std::fs::create_dir_all(&secret_dir).unwrap();
    let secret = secret_dir.join("secret.txt");
    std::fs::write(&secret, "DW46-LOCAL-SECRET-MARKER").unwrap();
    let local = format!(
        "file:///{}",
        secret
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
    );
    let trap = fx
        .app()
        .artifacts()
        .publish_evidence(
            &**fx.app().store(),
            Evidence {
                scope: ArtifactScope::Workspace,
                producer: user.principal().clone(),
                principal_kind: "Human".into(),
                agent_session: None,
                root: None,
                assignment: None,
                title: "trap.html".into(),
                media_type: "text/html".into(),
                tags: vec![],
                source: json!({"kind": "Fixture"}),
            },
            format!("<!doctype html><title>Trap</title><meta http-equiv=refresh content=\"0;url={local}\"><a href=\"{local}\">Local file</a><iframe src=\"{local}\"></iframe>").as_bytes(),
            |_, _| Ok(()),
        )
        .unwrap()
        .0;
    let s = fx.session(&session);
    let op = fx.accepted(&user, "browser.navigate", json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "targetId": target, "location": {"kind": "Artifact", "artifact": trap}}));
    let trapped = fx.snapshot(fx.completion(&user, &op));
    assert!(
        trapped
            .url
            .0
            .starts_with("https://artifact.devworks.invalid/"),
        "{}",
        trapped.url.0
    );
    fx.drive(1500);
    let (now, nax) = fresh(&target_json(&trapped));
    assert!(!nax.to_string().contains("DW46-LOCAL-SECRET"), "{nax}");
    if nax["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["name"] == "Local file")
    {
        let _ = fx.input(
            &user,
            &session,
            json!({"kind": "Click", "node": node(&now, &nax, "Local file"), "button": "Primary"}),
        );
        fx.drive(1500);
    }
    let (_, after) = fresh(&target_json(&now));
    assert!(!after.to_string().contains("DW46-LOCAL-SECRET"), "{after}");
    let _ = std::fs::remove_dir_all(&secret_dir);
    eprintln!("artifact origin: no local file readable");
    // A real navigation invalidates the previous document's nodes.
    let snap = fx.snapshot(fx.navigate(
        &user,
        &session,
        Some(&target),
        &format!("http://127.0.0.1:{port}/"),
    ));
    let ax = fx.json(&snap.accessibility);
    let go = node(&snap, &ax, "Go");
    fx.observation(fx.input(
        &user,
        &session,
        json!({"kind": "Click", "node": node(&snap, &ax, "Next page"), "button": "Primary"}),
    ));
    let s = fx.session(&session);
    let fault = fx.refused(&user, "browser.input", json!({"sessionId": session.as_str(), "expectedRevision": s.revision.to_string(), "action": {"kind": "Click", "node": go, "button": "Primary"}}));
    assert_eq!(fault.code, DomainErrorCode::SourceChanged);
    let closed = fx.close(&user, &session);
    assert_eq!(closed.phase, BrowserPhase::Closed);
    assert_eq!(NativeProcessHost.probe(&identity), Probe::Gone);
    assert!(!root.exists());
}

#[test]
fn hover_cannot_redirect_a_click_to_a_same_document_cover() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let snap = fx.snapshot(fx.navigate(&user, &session, Some("t1"), "https://app.test/form"));
    let ax = fx.json(&snap.accessibility);
    let b = fx.platform.browser(&session);
    let before = b.lock().unwrap().log.len();
    b.lock().unwrap().cover_on_move = true;
    let click = json!({"kind": "Click", "node": node(&snap, &ax, "Submit"), "button": "Primary"});
    assert_eq!(
        failed_kind(fx.input(&user, &session, click)),
        "browser_node"
    );
    let browser = b.lock().unwrap();
    let events: Vec<_> = browser.log[before..]
        .iter()
        .filter(|m| m["method"] == "Input.dispatchMouseEvent")
        .map(|m| m["params"]["type"].as_str().unwrap())
        .collect();
    assert_eq!(events, ["mouseMoved"]);
    assert!(
        !browser.cover_on_move,
        "hover ran and changed the hit target"
    );
}

#[test]
fn cancelling_an_undispatched_close_allows_replacement_cleanup() {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let fx = Fx::new();
    let user = fx.user.clone();
    let session = fx.create(&user, workspace());
    let identity = fx.identity(&session);
    let first = fx.accepted(&user,"browser.sessions",json!({"action":{"kind":"Close","sessionId":session.as_str(),"expectedRevision":fx.session(&session).revision.to_string()}}));
    let op = first.clone();
    let delivery = read(&**fx.app().store(), move |s| {
        s.queue().deliveries(
            &DeliveryFilter {
                consumer: None,
                operation_id: Some(op),
            },
            None,
            10,
        )
    })
    .entries
    .remove(0);
    assert_eq!(delivery.state, QueueState::Ready);
    let reply = fx.command(&user,"queue.resolve",&fx.key(),json!({"deliveryId":delivery.id.as_str(),"expectedRevision":delivery.revision.to_string(),"expectedFence":delivery.generation.to_string(),"resolution":"CancelBeforeDispatch","reason":"cancel queued close"}));
    assert!(
        matches!(Fx::decision(&reply), DomainDecision::Applied { .. }),
        "{reply:?}"
    );
    let op = first.clone();
    assert_eq!(
        read(&**fx.app().store(), move |s| s.operations().operation(&op))
            .unwrap()
            .state,
        OperationState::CancelledBeforeDispatch
    );
    assert_eq!(fx.session(&session).phase, BrowserPhase::Stopping);
    match fx.result(
        &user,
        "browser.sessions",
        json!({"scope": workspace(), "page": {"limit": 10}}),
    ) {
        OperationResult::BrowserSessions { value } => assert!(
            matches!(value.items.0[0].state, ItemState::Unavailable),
            "an unverified stop must remain closable in the native UI: {:?}",
            value.items.0[0].state
        ),
        other => panic!("{other:?}"),
    }
    let reply = fx.command(&user,"browser.sessions",&fx.key(),json!({"action":{"kind":"Close","sessionId":session.as_str(),"expectedRevision":fx.session(&session).revision.to_string()}}));
    let DomainDecision::Accepted {
        operation_id: second,
    } = Fx::decision(&reply)
    else {
        panic!("cancelled close must accept replacement cleanup: {reply:?}")
    };
    assert_ne!(second, &first);
    fx.succeeded(&user, second);
    assert_eq!(fx.session(&session).phase, BrowserPhase::Closed);
    assert!(matches!(NativeProcessHost.probe(&identity), Probe::Gone));
}

fn source_git(path: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(path)
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}
fn source_fixture(
    h: &Harness,
    store: &dyn ApplicationStore,
    owner: &ActorId,
) -> devworks_engine::git::GitComposition {
    use devworks_core::authority::destructive::FilesystemResolver;
    use devworks_core::protocol::data::{GitOid, ObjectFormat};
    use devworks_core::services::git::{GitConfig, RegisteredGitRoot};
    let primary = h.root.join("tested-primary");
    let owned = h.root.join("tested-owned");
    std::fs::create_dir(&primary).unwrap();
    std::fs::create_dir(&owned).unwrap();
    let primary = primary.canonicalize().unwrap();
    let owned = owned.canonicalize().unwrap();
    let tree = owned.join("tree");
    let tree_argument = devworks_core::services::indexing::git::path_argument(&tree).unwrap();
    source_git(&primary, &["init", "-b", "main"]);
    std::fs::write(primary.join("source.txt"), "initial\n").unwrap();
    source_git(&primary, &["add", "source.txt"]);
    source_git(&primary, &["commit", "-m", "initial"]);
    source_git(
        &primary,
        &["worktree", "add", "-b", "tested", &tree_argument, "HEAD"],
    );
    let fs = NativeFilesystem::new();
    let identity = |path: &str| {
        fs.resolve(path, None)
            .unwrap()
            .target
            .unwrap()
            .identity
            .unwrap()
    };
    let location = GitWorktreeLocation {
        root: identity(tree.to_str().unwrap()),
        git_dir: identity(&source_git(
            &tree,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        )),
        common_dir: identity(&source_git(
            &tree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )),
    };
    let record = StoredGitWorktree {
        id: id("tested-tree"),
        repository_id: id("tested-repository"),
        work_id: id("root-a"),
        owner: owner.clone(),
        ownership_revision: 1,
        owner_root_id: Some(devworks_core::protocol::RootId(id("root-a"))),
        owner_assignment_id: None,
        registered_root_id: id("tested-owned"),
        path: tree.to_str().unwrap().into(),
        branch: "refs/heads/tested".into(),
        head: GitOid {
            format: ObjectFormat::Sha1,
            hex: source_git(&tree, &["rev-parse", "HEAD"]),
        },
        revision: 1,
        removed: false,
        location: Some(location),
        pending_operation_id: None,
        last_operation_id: None,
        last_result: None,
    };
    let path = primary.to_str().unwrap().to_owned();
    run_write(store, WriteOptions::default(), move |tx| {
        tx.projects().insert_repository(&StoredRepository {
            id: id("tested-repository"),
            location: path.clone(),
            name: "Tested repository".into(),
            remote_url: None,
            primary: true,
            lifecycle: "Active".into(),
            languages: vec![],
            revision: 1,
            registered_at_ms: T0,
        })?;
        tx.projects()
            .set_members(&ProjectId(id("project-a")), &[id("tested-repository")])?;
        tx.work()
            .set_work_repositories(&id("root-a"), &[id("tested-repository")])?;
        tx.git().insert_git_worktree(&record)
    })
    .unwrap();
    devworks_engine::git::GitComposition {
        config: GitConfig {
            roots: vec![RegisteredGitRoot {
                id: id("tested-owned"),
                path: owned.to_str().unwrap().into(),
            }],
            filesystem: Some(Arc::new(fs)),
            ..GitConfig::default()
        },
    }
}
#[test]
fn browser_evidence_resolves_its_invocation_worktree_and_preserves_old_sources() {
    browser_source_case(false, false);
}
#[test]
fn browser_evidence_records_source_when_a_load_event_is_missing() {
    browser_source_case(false, true);
}
#[test]
fn installed_browser_records_real_worktree_provenance_when_opted_in() {
    if std::env::var("DEVWORKS_BROWSER_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    browser_source_case(true, false);
}
fn browser_source_case(native: bool, missing_load: bool) {
    let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let h = Harness::new();
    let tree = h.root.join("tested-owned/tree");
    let platform = Arc::new(FakePlatform::new(&h.root));
    let real: Arc<dyn BrowserPlatform> = if native {
        Arc::new(NativeBrowserPlatform::new(
            &h.root,
            std::env::var_os("DEVWORKS_BROWSER").map(PathBuf::from),
        ))
    } else {
        platform.clone()
    };
    let asset = real
        .assets()
        .into_iter()
        .find(|a| a.qualification == Qualification::Present)
        .unwrap()
        .id;
    // The fake's missing lifecycle notification must settle within the fixture's
    // completion bound. Production navigation alone may consume that entire bound.
    let browser = if native {
        BrowserConfig::default()
    } else {
        config()
    };
    let fx = Fx::boot_with_source(h, platform, real, browser, true);
    assert!(
        fx.app().git().is_some(),
        "Git source must qualify before browser admission"
    );
    let scoped = fx.user.clone().with_lineage(
        Some(ProjectId(id("project-a"))),
        Some(devworks_core::protocol::RootId(id("root-a"))),
        None,
    );
    let bound = devworks_core::services::git::context::bind_worktree(
        &**fx.app().store(),
        scoped.clone(),
        &id("tested-tree"),
    )
    .unwrap();
    let create_body = json!({"action":{"kind":"Create","scope":{"kind":"Project","projectId":"project-a"},"browserAssetId":asset}});
    let create_key = fx.key();
    let created = fx.command(&bound, "browser.sessions", &create_key, create_body.clone());
    let op = match Fx::decision(&created) {
        DomainDecision::Accepted { operation_id } => operation_id.clone(),
        other => panic!("{other:?}"),
    };
    let session = match fx.succeeded(&bound, &op) {
        OperationResult::BrowserSessions { value } => value.items.0[0].id.clone(),
        other => panic!("{other:?}"),
    };
    let original = fx.session(&session).tested_repository.unwrap();
    assert_eq!(original["dirty"], false);
    let initial_head = source_git(&tree, &["rev-parse", "HEAD"]);
    assert_eq!(original["head"]["hex"], initial_head);
    std::fs::write(tree.join("source.txt"), "changed\n").unwrap();
    if missing_load {
        fx.platform
            .browser(&session)
            .lock()
            .unwrap()
            .suppress_next_load = true;
    }
    let nav=fx.accepted(&bound,"browser.navigate",json!({"sessionId":session,"expectedRevision":fx.session(&session).revision.to_string(),"location":{"kind":"Blank"}}));
    let snap = fx.snapshot(fx.completion(&bound, &nav));
    let evidence = |artifact: &ArtifactRef| {
        read(&**fx.app().store(), |s| {
            s.artifacts().version(&artifact.version_id)
        })
        .unwrap()
        .source["source"]["testedRepository"]
            .clone()
    };
    let first = evidence(&snap.accessibility);
    assert_eq!(first["head"]["hex"], initial_head);
    assert_eq!(first["dirty"], true);
    source_git(&tree, &["commit", "-am", "changed"]);
    let next_head = source_git(&tree, &["rev-parse", "HEAD"]);
    assert_ne!(initial_head, next_head);
    let screenshot = match fx.result(
        &bound,
        "browser.screenshot",
        json!({"target":snap.target,"fullPage":false}),
    ) {
        OperationResult::BrowserScreenshot { value } => value,
        other => panic!("{other:?}"),
    };
    let second = evidence(&screenshot);
    assert_eq!(second["head"]["hex"], next_head);
    assert_eq!(second["dirty"], false);
    // The same immutable source is available over the public artifact byte-read contract,
    // including image streams, rather than only through the internal storage repository.
    let read_source = |artifact: &ArtifactRef| {
        let frame = json!({"protocol":"1.0-draft.1","schemaRevision":4,"queryId":fx.key(),"query":{"kind":"Read","artifactId":artifact.artifact_id,"versionId":artifact.version_id},"limit":1});
        match fx
            .app()
            .query(&scoped, "artifacts.read", frame.to_string().as_bytes())
            .unwrap()
        {
            DomainQueryReply::Result {
                result: OperationResult::ArtifactsRead { value },
                ..
            } => match value {
                devworks_core::protocol::records::ArtifactReadContent::Text {
                    tested_repository,
                    ..
                }
                | devworks_core::protocol::records::ArtifactReadContent::Binary {
                    tested_repository,
                    ..
                } => serde_json::to_value(tested_repository).unwrap(),
            },
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(read_source(&screenshot), second);
    assert_eq!(read_source(&snap.accessibility), first);
    assert_eq!(
        evidence(&snap.accessibility),
        first,
        "old immutable evidence retains its original observation"
    );
    assert_eq!(
        fx.session(&session).tested_repository.unwrap(),
        original,
        "session start source never follows later commits"
    );
    let unbound = match fx.result(
        &scoped,
        "browser.screenshot",
        json!({"target":snap.target,"fullPage":false}),
    ) {
        OperationResult::BrowserScreenshot { value } => value,
        other => panic!("{other:?}"),
    };
    assert!(
        evidence(&unbound).is_null(),
        "an unbound invocation never borrows the session's source"
    );
    let frame = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":3,"queryId":"source-list","body":{"kind":"browser.sessions","scope":{"kind":"Project","projectId":"project-a"},"page":{"limit":10}}});
    let result = fx
        .app()
        .query(&scoped, "browser.sessions", frame.to_string().as_bytes())
        .unwrap();
    assert!(
        serde_json::to_value(result)
            .unwrap()
            .to_string()
            .contains(&initial_head)
    );
    let second_create = fx.accepted(&bound, "browser.sessions", create_body.clone());
    let second_session = match fx.succeeded(&bound, &second_create) {
        OperationResult::BrowserSessions { value } => value.items.0[0].id.clone(),
        other => panic!("{other:?}"),
    };
    let git_launches = || {
        let connection = rusqlite::Connection::open(fx.h().root.join("workspace.sqlite")).unwrap();
        connection
            .query_row(
                "SELECT count(*) FROM owned_processes WHERE lower(program) IN ('git','git.exe')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    let before_close = git_launches();
    fx.close(&scoped, &second_session);
    assert_eq!(
        git_launches(),
        before_close,
        "Close never starts a Git helper, even with a valid source binding"
    );
    let store = fx.app().store();
    let host = store.info().host_actor;
    run_write(&**store, WriteOptions::default(), move |tx| {
        let mut row = tx.git().git_worktree(&id("tested-tree"))?.unwrap();
        let revision = row.revision;
        row.revision += 1;
        row.ownership_revision += 1;
        row.owner = host.clone();
        row.owner_root_id = None;
        row.owner_assignment_id = None;
        tx.git().update_git_worktree(&row, revision)
    })
    .unwrap();
    assert_eq!(
        fx.fault(
            &bound,
            "browser.screenshot",
            json!({"target":snap.target,"fullPage":false})
        )
        .code,
        DomainErrorCode::SourceChanged
    );
    let replay = fx.command(&bound, "browser.sessions", &create_key, create_body);
    assert!(
        matches!(replay, CommandReply::Recorded { replayed: true, .. }),
        "a source refusal never loses an earlier receipt"
    );
    fx.close(&scoped, &session);
}
