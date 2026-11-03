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

/// Runs frames until every request the shell made has been answered and applied, then lets
/// the UI settle (which also runs the accessibility check: every input has a name).
pub fn settle(h: &mut Harness<'_, Shell>) {
    for _ in 0..40 {
        h.step();
        assert!(h.state().wait_idle(), "requests finish");
        h.step();
        if h.state().client().is_none_or(|c| c.activities().is_empty()) {
            break;
        }
    }
    h.run();
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
