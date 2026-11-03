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
            Ok(_) => return,
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

pub fn click(h: &mut Harness<'_, Shell>, role: egui::accesskit::Role, label: &str) {
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
    open: Arc<(Mutex<bool>, std::sync::Condvar)>,
}
impl Gate {
    pub fn new(operation: &'static str) -> Self {
        Self {
            operation,
            open: Arc::new((Mutex::new(true), std::sync::Condvar::new())),
        }
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
    fn wait(&self, operation: &str) {
        if operation == self.operation {
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
        self.gate.wait(operation);
        self.inner.query(operation, frame)
    }
    fn command(&self, operation: &str, frame: &[u8]) -> devworks_core::gateway::CommandOutcome {
        self.gate.wait(operation);
        self.inner.command(operation, frame)
    }
    fn mint_workspace_upload(&self, file: &Path) -> Result<devworks_core::protocol::Id, String> {
        self.inner.mint_workspace_upload(file)
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
    fn read_stream(
        &self,
        stream: &devworks_core::protocol::Id,
        max: u64,
    ) -> Result<Vec<u8>, String> {
        self.inner.read_stream(stream, max)
    }
}
