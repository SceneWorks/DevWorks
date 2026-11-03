//! The DevWorks desktop binary: resolves the data root, starts the engine through the one
//! composition root (recovery before any UI command), and runs the native egui window. Closing
//! the window runs the bounded runtime shutdown; the process then exits.
//!
//! Options: `--data-dir <path>` (or `DEVWORKS_DATA_DIR`) selects the data root. Two smoke-check
//! aids exercise boot, rendering and shutdown without interaction: `--screenshot <file.png>`
//! saves the window's rendered frame two seconds after launch, and `--close-after-secs <n>`
//! requests the normal window close (the same lifecycle as the close button) after `n` seconds.
use devworks_desktop::host::{DesktopConfig, Engine};
use devworks_desktop::platform::default_data_root;
use devworks_desktop::shell::{HostControl, NativeDialogs, Shell, ShellInit};
use devworks_desktop::{VERSION, memory};
use eframe::egui;
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct App {
    shell: Shell,
    launched: Instant,
    close_at: Option<Instant>,
    screenshot: Option<PathBuf>,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.launched.elapsed() >= Duration::from_secs(2)
            && let Some(path) = self.screenshot.take()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(
                egui::ScreenshotCallback::new(move |image| save_png(&image, &path)),
            ));
        }
        if self.screenshot.is_some() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        if let Some(at) = self.close_at {
            if Instant::now() >= at {
                self.close_at = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(at.saturating_duration_since(Instant::now()));
            }
        }
        self.shell.ui(ui);
    }
    fn on_exit(&mut self) {
        self.shell.on_exit();
        eprintln!(
            "devworks: window closed ({})",
            self.shell
                .shutdown_summary()
                .unwrap_or("the runtime shutdown completes as the process exits")
        );
    }
}

fn save_png(image: &egui::ColorImage, path: &std::path::Path) {
    let [w, h] = image.size;
    let rgba: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|c| c.to_srgba_unmultiplied())
        .collect();
    let saved = image::RgbaImage::from_raw(w as u32, h as u32, rgba)
        .ok_or_else(|| "empty frame".to_owned())
        .and_then(|b| b.save(path).map_err(|e| e.to_string()));
    match saved {
        Ok(()) => eprintln!("devworks: screenshot saved to {}", path.display()),
        Err(e) => eprintln!("devworks: screenshot failed: {e}"),
    }
}

fn arg(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == name {
            return args.next();
        }
    }
    None
}

fn main() -> eframe::Result {
    let data_root = arg("--data-dir")
        .map(PathBuf::from)
        .or_else(default_data_root);
    let close_at = arg("--close-after-secs")
        .and_then(|s| s.parse::<u64>().ok())
        .map(|s| Instant::now() + Duration::from_secs(s));
    let screenshot = arg("--screenshot").map(PathBuf::from);
    let (backend, control, memory_path): (_, Option<Box<dyn HostControl>>, _) = match &data_root {
        None => (
            Err(
                "no home directory is known, so there is no data root; set DEVWORKS_DATA_DIR."
                    .to_owned(),
            ),
            None,
            None,
        ),
        Some(root) => {
            let started = Instant::now();
            match Engine::start(DesktopConfig::new(root)) {
                Ok(engine) => {
                    let r = engine.host().report();
                    eprintln!(
                        "devworks: runtime booted in {} ms at {} (recovery: {} queue claims requeued, {} held; {} staging operations completed, {} failed; keep-awake {:?})",
                        started.elapsed().as_millis(),
                        root.display(),
                        r.queue.requeued.len(),
                        r.queue.held.len(),
                        r.artifacts.completed.len(),
                        r.artifacts.failed.len(),
                        r.keep_awake,
                    );
                    (
                        Ok(engine.backend()),
                        Some(Box::new(engine) as Box<dyn HostControl>),
                        Some(root.join(memory::FILE)),
                    )
                }
                Err(e) => {
                    eprintln!("devworks: runtime did not start: {e}");
                    (Err(e.to_string()), None, None)
                }
            }
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DevWorks")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([760.0, 520.0]),
        ..Default::default()
    };
    let shell = Shell::new(ShellInit {
        backend,
        dialogs: Box::new(NativeDialogs),
        memory_path,
        control,
        version: VERSION.to_owned(),
    });
    let result = eframe::run_native(
        "DevWorks",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(App {
                shell,
                launched: Instant::now(),
                close_at,
                screenshot,
            }))
        }),
    );
    eprintln!("devworks: exited");
    result
}
