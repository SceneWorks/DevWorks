//! The DevWorks desktop binary: resolves the data root, starts the engine through the one
//! composition root (recovery before any UI command), and runs the native egui window. Closing
//! the window runs the bounded runtime shutdown; the process then exits.
//!
//! Options: `--data-dir <path>` (or `DEVWORKS_DATA_DIR`) selects the data root. Two smoke-check
//! aids exercise boot, rendering and shutdown without interaction: `--screenshot <file.png>`
//! saves the window's rendered frame two seconds after launch, and `--close-after-secs <n>`
//! requests the normal window close (the same lifecycle as the close button) after `n` seconds.
use devworks_core::services::update::{InstallOutcome, UpdateService};
use devworks_desktop::app_updates::{self, Candidate};
use devworks_desktop::host::{DesktopConfig, Engine};
use devworks_desktop::platform::default_data_root;
use devworks_desktop::shell::{HostControl, NativeDialogs, Shell, ShellInit};
use devworks_desktop::{VERSION, memory};
use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct App {
    shell: Shell,
    launched: Instant,
    close_at: Option<Instant>,
    screenshot: Option<PathBuf>,
    candidate: Option<Candidate>,
    update_service: Option<Arc<UpdateService>>,
    install_gate: Option<std::fs::File>,
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
        if let Some(mut candidate) = self.candidate.take() {
            let approval = candidate.approval().clone();
            let healthy = candidate.healthy();
            self.install_gate = candidate.take_gate();
            match healthy {
                Ok(()) => {
                    if let Some(service) = self.update_service.take() {
                        let _ = service.finish(&approval, InstallOutcome::Installed, "The signed build started with matching offline Help. Held work needs Continue.");
                    }
                }
                Err(error) => {
                    eprintln!("DevWorks update startup held: {error}");
                    self.shell.request_close();
                }
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
    #[cfg(any(unix, windows))]
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "mcp") {
        let data_root = arg("--data-dir")
            .map(PathBuf::from)
            .or_else(default_data_root);
        let result = data_root
            .as_deref()
            .ok_or_else(|| std::io::Error::other("no DevWorks data root is configured"))
            .and_then(devworks_desktop::run_mcp_bridge);
        if let Err(error) = result {
            eprintln!("devworks mcp: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if std::env::args_os()
        .skip(1)
        .eq([std::ffi::OsStr::new("--release-evidence")])
    {
        println!("{}", app_updates::release_evidence());
        return Ok(());
    }
    if let Some(result) = app_updates::run_internal() {
        if let Err(error) = result {
            eprintln!("DevWorks updater: {error}");
        }
        return Ok(());
    }
    let startup = app_updates::enter_application();
    let (gate, mut candidate, recovery_profile, install_error) = match startup {
        Ok(startup) => (
            startup.gate,
            startup.candidate,
            startup.recovery_profile,
            None,
        ),
        Err(error) => (None, None, None, Some(error)),
    };
    let data_root = candidate
        .as_ref()
        .map(|candidate| candidate.profile().to_owned())
        .or(recovery_profile)
        .or_else(|| arg("--data-dir").map(PathBuf::from))
        .or_else(default_data_root);
    let close_at = arg("--close-after-secs")
        .and_then(|s| s.parse::<u64>().ok())
        .map(|s| Instant::now() + Duration::from_secs(s));
    let screenshot = arg("--screenshot").map(PathBuf::from);
    let mut update_service = None;
    let (backend, control, memory_path): (_, Option<Box<dyn HostControl>>, _) =
        if let Some(error) = install_error {
            (Err(error), None, None)
        } else {
            match &data_root {
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
                let mut config = DesktopConfig::new(root);
                let guarded = candidate.take().map(|candidate| {
                    config.app_update = Some(candidate.approval().clone());
                    let guarded = Arc::new(std::sync::Mutex::new(Some(candidate)));
                    let current = guarded.clone();
                    config.startup_admission =
                        Some(devworks_engine::runtime::StartupAdmission::new(move || {
                            current
                                .lock()
                                .map_err(|_| "The update startup owner was interrupted.")?
                                .as_mut()
                                .ok_or("The update startup owner is missing.")?
                                .admit()
                        }));
                    guarded
                });
                let started_engine = Engine::start(config);
                candidate = guarded.and_then(|holder| holder.lock().ok()?.take());
                match started_engine {
                    Ok(engine) => {
                        if candidate.is_some() {
                            update_service = Some(engine.host().updates().clone());
                        }
                        let r = engine.host().report();
                        eprintln!(
                            "devworks: runtime booted in {} ms at {} (recovery: {} queue claims \
                            requeued, {} held; {} staging operations completed, {} failed; \
                            keep-awake {:?})",
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
                        if candidate.is_some() {
                            return Ok(());
                        }
                        (Err(e.to_string()), None, None)
                    }
                }
            }
        }
        };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DevWorks")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size(devworks_desktop::theme::MIN_WINDOW),
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
                candidate,
                update_service,
                install_gate: gate,
            }))
        }),
    );
    eprintln!("devworks: exited");
    result
}
