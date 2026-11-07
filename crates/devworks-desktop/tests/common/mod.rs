//! Shared fixtures: the real composition root (SQLite store, managed files, native process and
//! power adapters) booted in a temporary data root, the shell driven headlessly by
//! `egui_kittest`, and scripted native dialogs. No window, GPU, inference or network.
#![allow(dead_code)]
use devworks_desktop::backend::Backend;
use devworks_desktop::host::{DesktopConfig, Engine};
use devworks_desktop::memory;
use devworks_desktop::shell::{Dialogs, HostControl, Shell, ShellInit};
use eframe::egui;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Heavy native cases share a process budget. Acquire once at case entry, before any boot,
/// and retain the guard through every runtime, harness and temporary-root cleanup. A case
/// may deliberately own multiple apps; acquiring per Engine would deadlock those scenarios.
#[must_use = "retain the native-case guard through case cleanup"]
pub struct NativeRuntimeCase {
    _owned: (),
}
static NATIVE_CASES: (Mutex<usize>, std::sync::Condvar) =
    (Mutex::new(0), std::sync::Condvar::new());
const NATIVE_CASE_LIMIT: usize = 2;

pub fn native_runtime_case() -> NativeRuntimeCase {
    let mut active = NATIVE_CASES.0.lock().unwrap_or_else(|p| p.into_inner());
    while *active >= NATIVE_CASE_LIMIT {
        active = NATIVE_CASES
            .1
            .wait(active)
            .unwrap_or_else(|p| p.into_inner());
    }
    *active += 1;
    NativeRuntimeCase { _owned: () }
}
impl Drop for NativeRuntimeCase {
    fn drop(&mut self) {
        let mut active = NATIVE_CASES.0.lock().unwrap_or_else(|p| p.into_inner());
        *active -= 1;
        NATIVE_CASES.1.notify_one();
    }
}

/// A temporary data root, removed when dropped.
pub struct Root(pub PathBuf);
impl Root {
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "devworks-desktop-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    pub fn memory(&self) -> PathBuf {
        self.0.join(memory::FILE)
    }
    /// A file beside (not inside) the data root.
    pub fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let dir = self.0.with_extension("files");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }
    pub fn dir(&self, name: &str) -> PathBuf {
        let p = self.0.with_extension("dirs").join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        let _ = std::fs::remove_dir_all(self.0.with_extension("files"));
        let _ = std::fs::remove_dir_all(self.0.with_extension("dirs"));
    }
}

pub fn config(root: &Path) -> DesktopConfig {
    let mut c = DesktopConfig::new(root);
    c.user_name = "Test user".into();
    c.shutdown_bound = Duration::from_secs(5);
    // Tests never scan the user's real repository base directory.
    c.indexing.discover_at_boot = false;
    // Never the real OS secret store or the network.
    c.secrets = Some(Arc::new(
        devworks_core::ports::secrets::SessionSecrets::new(),
    ));
    c.http = Some(Arc::new(devworks_core::ports::http::NoNetwork));
    c
}

pub fn start(root: &Root) -> Engine {
    Engine::start(config(&root.0)).expect("the runtime boots")
}

/// Scripted dialog results, consumed in order.
#[derive(Clone, Default)]
pub struct Script {
    pub files: Arc<Mutex<Vec<Vec<PathBuf>>>>,
    pub folders: Arc<Mutex<Vec<PathBuf>>>,
}
impl Dialogs for Script {
    fn pick_files(&mut self) -> Vec<PathBuf> {
        let mut f = self.files.lock().unwrap();
        if f.is_empty() {
            Vec::new()
        } else {
            f.remove(0)
        }
    }
    fn pick_folder(&mut self, _: Option<&str>) -> Option<PathBuf> {
        let mut f = self.folders.lock().unwrap();
        (!f.is_empty()).then(|| f.remove(0))
    }
}

pub fn shell_with(
    backend: Result<Arc<dyn Backend>, String>,
    root: &Root,
    dialogs: Script,
    control: Option<Box<dyn HostControl>>,
) -> Shell {
    Shell::new(ShellInit {
        backend,
        dialogs: Box::new(dialogs),
        memory_path: Some(root.memory()),
        control,
        version: "0.1.0-test".into(),
    })
}

pub fn shell(engine: &Engine, root: &Root, dialogs: Script) -> Shell {
    shell_with(Ok(engine.backend()), root, dialogs, None)
}

pub fn harness(shell: Shell, w: f32, h: f32) -> Harness<'static, Shell> {
    Harness::builder()
        .with_size(egui::vec2(w, h))
        .with_max_steps(12)
        .build_ui_state(|ui, shell: &mut Shell| shell.ui(ui), shell)
}

/// Longest time [`settle`] waits for the shell to stop repainting. A UI that never stops (a
/// real repaint loop) fails at this bound with egui's repaint causes, never silently.
pub const SETTLE_BOUND: Duration = Duration::from_secs(30);

/// Runs frames until every request the shell made has been answered and applied, then lets
/// the UI settle (which also runs the accessibility check: every input has a name).
///
/// Frames are not timed, but replies arrive from worker threads: while a request is in
/// flight its spinner repaints every frame, so under load `Harness::run` can use up its frame
/// budget before the reply lands (seen as "would have settled after N steps" with the spinner
/// as the cause). A request can also start during that run (a runtime hint re-queries the
/// visible transcript). So a run that exceeds the budget is retried after the requests in
/// flight finish, until [`SETTLE_BOUND`]; only then is it a failure.
pub fn settle(h: &mut Harness<'_, Shell>) {
    let deadline = std::time::Instant::now() + SETTLE_BOUND;
    loop {
        for _ in 0..40 {
            h.step();
            assert!(h.state().wait_idle(), "requests finish");
            h.step();
            if h.state().client().is_none_or(|c| c.activities().is_empty()) {
                break;
            }
        }
        match h.try_run() {
            Ok(_) => {
                // A final frame may submit a query after its spinner/status bar rendered.
                // Its worker has not necessarily sent the repaint wake yet: a quiet egui
                // frame alone does not mean its backend replies were applied.
                if h.state().client().is_none_or(|c| c.activities().is_empty()) {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "requests never settled within {SETTLE_BOUND:?}"
                );
            }
            Err(e) if std::time::Instant::now() < deadline => {
                let _ = e;
            }
            Err(e) => panic!("the shell never stopped repainting within {SETTLE_BOUND:?}: {e}"),
        }
    }
}

/// Labels of every node with `role` in tree order.
pub fn labels(h: &Harness<'_, Shell>, role: egui::accesskit::Role) -> Vec<String> {
    h.query_all_by_role(role)
        .filter_map(|n| n.accesskit_node().label())
        .collect()
}

pub fn has_text(h: &Harness<'_, Shell>, text: &str) -> bool {
    h.query_all_by_label_contains(text).next().is_some()
}

/// Whether a toggle-like control (selectable tab, toggle button) is on.
pub fn is_on(n: &egui_kittest::Node<'_>) -> bool {
    n.accesskit_node().toggled() == Some(egui::accesskit::Toggled::True)
}

/// A left-navigation button (the toolbar repeats some labels, such as Help).
pub fn nav<'t>(h: &'t Harness<'_, Shell>, label: &'t str) -> egui_kittest::Node<'t> {
    h.query_all_by_role_and_label(egui::accesskit::Role::Button, label)
        .find(|n| n.rect().right() <= devworks_desktop::theme::NAV + 1.0)
        .unwrap_or_else(|| panic!("navigation button {label}"))
}

pub fn click_nav(h: &mut Harness<'_, Shell>, label: &str) {
    nav(h, label).click();
    settle(h);
}

/// Scrolls the control into view (as a user would) and clicks it after layout converges.
/// A fixed frame count can leave deferred scroll/layout changes beneath the click.
pub fn click(h: &mut Harness<'_, Shell>, role: egui::accesskit::Role, label: &str) {
    // Direct state injection must be rendered before querying the target's geometry.
    settle(h);
    h.get_by_role_and_label(role, label).scroll_to_me();
    settle(h);
    h.get_by_role_and_label(role, label).click();
    settle(h);
}

/// A dropped file for `RawInput::dropped_files`.
#[derive(Debug)]
pub struct Dropped(pub PathBuf);
impl egui::DroppedFile for Dropped {
    fn path(&self) -> &Path {
        &self.0
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.0).map_err(|e| e.to_string())
    }
}

/// A tiny valid PNG (2×2 opaque blue).
pub fn png() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 255, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// A gate on one operation of a real backend: while closed, that operation waits (a request
/// "in flight"); everything else passes straight through.
#[derive(Clone)]
pub struct Gate {
    operation: &'static str,
    frame_field: Option<&'static str>,
    open: Arc<(Mutex<bool>, std::sync::Condvar)>,
    /// Hold the reply after the request ran (its snapshot or commit is already taken), rather
    /// than the request before it runs.
    after: bool,
    /// Forward the runtime's change hints (off: every hint is missed, so only the shell's own
    /// bookkeeping can repair a stale reply).
    hints: bool,
    fail_query: Arc<std::sync::atomic::AtomicBool>,
}
impl Gate {
    pub fn new(operation: &'static str) -> Self {
        Self {
            operation,
            frame_field: None,
            open: Arc::new((Mutex::new(true), std::sync::Condvar::new())),
            after: false,
            hints: true,
            fail_query: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
    /// A gate that lets the request run and holds only its reply (a slow response).
    pub fn holding_replies(operation: &'static str) -> Self {
        Self {
            after: true,
            ..Self::new(operation)
        }
    }
    /// The same gate over a backend whose change hints never arrive.
    pub fn missing_hints(self) -> Self {
        Self {
            hints: false,
            ..self
        }
    }
    /// Hold only frames containing this JSON-pointer field (for example a later-page cursor).
    pub fn with_frame_field(self, pointer: &'static str) -> Self {
        Self {
            frame_field: Some(pointer),
            ..self
        }
    }
    /// Refuse the next matching query while preserving the real backend for other work.
    pub fn fail_next_query(&self) {
        self.fail_query.store(true, Ordering::SeqCst);
    }
    pub fn close(&self) {
        *self.open.0.lock().unwrap() = false;
    }
    pub fn open(&self) {
        *self.open.0.lock().unwrap() = true;
        self.open.1.notify_all();
    }
    pub fn over(&self, inner: Arc<dyn Backend>) -> Arc<dyn Backend> {
        Arc::new(Gated {
            inner,
            gate: self.clone(),
        })
    }
    fn wait(&self, operation: &str, frame: &[u8]) {
        let matches_frame = self.frame_field.is_none_or(|pointer| {
            serde_json::from_slice::<serde_json::Value>(frame)
                .is_ok_and(|v| v.pointer(pointer).is_some())
        });
        if operation == self.operation && matches_frame {
            let mut open = self.open.0.lock().unwrap();
            while !*open {
                open = self.open.1.wait(open).unwrap();
            }
        }
    }
}

struct Gated {
    inner: Arc<dyn Backend>,
    gate: Gate,
}
impl Backend for Gated {
    fn query(&self, operation: &str, frame: &[u8]) -> devworks_core::gateway::QueryOutcome {
        if operation == self.gate.operation && self.gate.fail_query.swap(false, Ordering::SeqCst) {
            return Err(devworks_core::gateway::GatewayFailure::NoUseCase(
                "controlled query fault".into(),
            ));
        }
        if !self.gate.after {
            self.gate.wait(operation, frame);
        }
        let out = self.inner.query(operation, frame);
        if self.gate.after {
            self.gate.wait(operation, frame);
        }
        out
    }
    fn command(&self, operation: &str, frame: &[u8]) -> devworks_core::gateway::CommandOutcome {
        if !self.gate.after {
            self.gate.wait(operation, frame);
        }
        let out = self.inner.command(operation, frame);
        if self.gate.after {
            self.gate.wait(operation, frame);
        }
        out
    }
    fn human_decision(&self, frame: &[u8]) -> devworks_core::gateway::CommandOutcome {
        let operation = "approvals.record_human_decision";
        if !self.gate.after {
            self.gate.wait(operation, frame);
        }
        let out = self.inner.human_decision(frame);
        if self.gate.after {
            self.gate.wait(operation, frame);
        }
        out
    }
    fn refresh_packages(&self) -> Result<(), String> {
        self.inner.refresh_packages()
    }
    fn fetch_index_bundle(&self, package: &str, digest: &str) -> Result<PathBuf, String> {
        self.inner.fetch_index_bundle(package, digest)
    }
    fn stage_api_key(
        &self,
        key: devworks_core::ports::secrets::Secret,
    ) -> Result<devworks_core::protocol::Id, String> {
        self.inner.stage_api_key(key)
    }
    fn secret_store(&self) -> devworks_desktop::backend::SecretStoreStatus {
        self.inner.secret_store()
    }
    fn mint_workspace_upload(&self, file: &Path) -> Result<devworks_core::protocol::Id, String> {
        self.inner.mint_workspace_upload(file)
    }
    fn mint_project_upload(
        &self,
        project: &devworks_core::protocol::ProjectId,
        file: &std::path::Path,
    ) -> Result<devworks_core::protocol::Id, String> {
        self.inner.mint_project_upload(project, file)
    }
    fn keep_awake_health(&self) -> devworks_core::settings::view::Health {
        self.inner.keep_awake_health()
    }
    fn hints(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.gate.hints.then(|| self.inner.hints()).flatten()
    }
    fn set_waker(&self, waker: devworks_desktop::backend::Waker) {
        self.inner.set_waker(waker)
    }
    fn token(&self, prefix: &str) -> String {
        self.inner.token(prefix)
    }
    fn scratch_dir(&self) -> PathBuf {
        self.inner.scratch_dir()
    }
    fn stage_pasted_image(
        &self,
        session: Option<&devworks_core::protocol::Id>,
        png: &[u8],
    ) -> Result<PathBuf, String> {
        self.inner.stage_pasted_image(session, png)
    }
    fn stage_project_text(
        &self,
        project: Option<&devworks_core::protocol::ProjectId>,
        extension: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, String> {
        self.inner.stage_project_text(project, extension, bytes)
    }
    fn read_stream(
        &self,
        stream: &devworks_core::protocol::Id,
        max: u64,
    ) -> Result<Vec<u8>, String> {
        self.inner.read_stream(stream, max)
    }
}

/// A real backend that loses the acknowledgment of the next `losses` admissions of one
/// command: each still runs (and commits) but replies `CommandStatusUnknown`, as after a
/// crash between commit and reply. Every frame it saw is recorded.
pub struct LoseAcks {
    inner: Arc<dyn Backend>,
    operation: &'static str,
    /// Lose the request itself (it never runs), not only its acknowledgment.
    requests: bool,
    losses: std::sync::atomic::AtomicUsize,
    pub frames: Mutex<Vec<Vec<u8>>>,
}
impl LoseAcks {
    pub fn over(inner: Arc<dyn Backend>, operation: &'static str, losses: usize) -> Arc<Self> {
        Arc::new(Self {
            inner,
            operation,
            requests: false,
            losses: std::sync::atomic::AtomicUsize::new(losses),
            frames: Mutex::new(Vec::new()),
        })
    }
    /// Loses the first `losses` requests before they reach the runtime (nothing commits).
    pub fn losing_requests(
        inner: Arc<dyn Backend>,
        operation: &'static str,
        losses: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            operation,
            requests: true,
            losses: std::sync::atomic::AtomicUsize::new(losses),
            frames: Mutex::new(Vec::new()),
        })
    }
    /// Request keys of every admission of the command, in order.
    pub fn request_keys(&self) -> Vec<String> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .map(|f| {
                serde_json::from_slice::<serde_json::Value>(f).unwrap()["requestKey"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }
}
impl Backend for LoseAcks {
    fn query(&self, operation: &str, frame: &[u8]) -> devworks_core::gateway::QueryOutcome {
        self.inner.query(operation, frame)
    }
    fn command(&self, operation: &str, frame: &[u8]) -> devworks_core::gateway::CommandOutcome {
        if operation != self.operation {
            return self.inner.command(operation, frame);
        }
        self.frames.lock().unwrap().push(frame.to_vec());
        let lose = self
            .losses
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if !lose || !self.requests {
            let out = self.inner.command(operation, frame);
            if !lose {
                return out;
            }
        }
        Ok(
            serde_json::from_value(serde_json::json!({"kind": "CommandStatusUnknown",
            "protocol": "1.0-draft.1", "requestKey": "lost", "correlationId": "lost"}))
            .unwrap(),
        )
    }
    fn human_decision(&self, frame: &[u8]) -> devworks_core::gateway::CommandOutcome {
        if self.operation != "approvals.record_human_decision" {
            return self.inner.human_decision(frame);
        }
        self.frames.lock().unwrap().push(frame.to_vec());
        let lose = self
            .losses
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if !lose || !self.requests {
            let out = self.inner.human_decision(frame);
            if !lose {
                return out;
            }
        }
        Ok(serde_json::from_value(serde_json::json!({"kind":"CommandStatusUnknown","protocol":"1.0-draft.1","requestKey":"lost","correlationId":"lost"})).unwrap())
    }
    fn refresh_packages(&self) -> Result<(), String> {
        self.inner.refresh_packages()
    }
    fn fetch_index_bundle(&self, package: &str, digest: &str) -> Result<PathBuf, String> {
        self.inner.fetch_index_bundle(package, digest)
    }
    fn stage_api_key(
        &self,
        key: devworks_core::ports::secrets::Secret,
    ) -> Result<devworks_core::protocol::Id, String> {
        self.inner.stage_api_key(key)
    }
    fn secret_store(&self) -> devworks_desktop::backend::SecretStoreStatus {
        self.inner.secret_store()
    }
    fn mint_workspace_upload(&self, file: &Path) -> Result<devworks_core::protocol::Id, String> {
        self.inner.mint_workspace_upload(file)
    }
    fn mint_project_upload(
        &self,
        project: &devworks_core::protocol::ProjectId,
        file: &std::path::Path,
    ) -> Result<devworks_core::protocol::Id, String> {
        self.inner.mint_project_upload(project, file)
    }
    fn keep_awake_health(&self) -> devworks_core::settings::view::Health {
        self.inner.keep_awake_health()
    }
    fn hints(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.inner.hints()
    }
    fn set_waker(&self, waker: devworks_desktop::backend::Waker) {
        self.inner.set_waker(waker)
    }
    fn token(&self, prefix: &str) -> String {
        self.inner.token(prefix)
    }
    fn scratch_dir(&self) -> PathBuf {
        self.inner.scratch_dir()
    }
    fn stage_pasted_image(
        &self,
        session: Option<&devworks_core::protocol::Id>,
        png: &[u8],
    ) -> Result<PathBuf, String> {
        self.inner.stage_pasted_image(session, png)
    }
    fn stage_project_text(
        &self,
        project: Option<&devworks_core::protocol::ProjectId>,
        extension: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, String> {
        self.inner.stage_project_text(project, extension, bytes)
    }
    fn read_stream(
        &self,
        stream: &devworks_core::protocol::Id,
        max: u64,
    ) -> Result<Vec<u8>, String> {
        self.inner.read_stream(stream, max)
    }
}

/// Steps (with a short pause, failure-only bounded) until `done` holds.
pub fn step_until(h: &mut Harness<'_, Shell>, what: &str, done: impl Fn(&Shell) -> bool) {
    let deadline = std::time::Instant::now() + SETTLE_BOUND;
    while !done(h.state()) {
        assert!(
            std::time::Instant::now() < deadline,
            "{what} never happened; route={:?}; help_scope={:?}; help_selected={:?}; help_loading={}; help_reading={}; help_error={:?}; help_read_error={:?}; help_notice={:?}; database_selected={:?}; database_busy={}; database_loading={}; database_error={:?}; database_notice={:?}; database_sources={:?}",
            h.state().route(),
            h.state().help.scope,
            h.state().help.selected,
            h.state().help.loading,
            h.state().help.reading,
            h.state().help.error,
            h.state().help.read_error,
            h.state().help.notice,
            h.state().databases.selected,
            h.state().databases.busy,
            h.state().databases.loading,
            h.state().databases.error,
            h.state().databases.notice,
            h.state().databases.connections
        );
        h.step();
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Workspace sessions as the runtime lists them.
pub fn listed_sessions(backend: &dyn Backend) -> Vec<String> {
    devworks_desktop::ops::sessions(backend.query(
        "sessions.list",
        &devworks_desktop::ops::sessions_list(backend),
    ))
    .unwrap()
    .0
    .into_iter()
    .map(|s| s.id.as_str().to_owned())
    .collect()
}
