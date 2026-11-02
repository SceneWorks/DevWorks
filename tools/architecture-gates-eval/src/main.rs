//! Disposable native feasibility checks. Fixture secrets only; no inference.
use serde_json::json;
use std::{
    error::Error,
    process::{Command, Stdio},
    time::Duration,
};

fn main() -> Result<(), Box<dyn Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("ui") => ui(),
        Some("secret") => secret(),
        Some("process") => process(),
        Some("worker") => worker(),
        Some("leaf") => {
            std::thread::sleep(Duration::from_secs(15));
            Ok(())
        }
        _ => Err("expected ui, secret or process".into()),
    }
}

#[derive(Default)]
struct Probe {
    frames: usize,
}
impl eframe::App for Probe {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _: &mut eframe::Frame) {
        use eframe::egui;
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Owned invisible feasibility window");
            let label = ui.label("Fixture draft");
            let mut draft = "No provider request".to_owned();
            ui.text_edit_multiline(&mut draft).labelled_by(label.id);
            ui.button("Continue fixture")
                .on_hover_text("Ordinary host command");
        });
        self.frames += 1;
        if self.frames == 2 {
            println!(
                "{}",
                json!({"passed":true,"frames":self.frames,"renderer":"glow","visible":false,"accesskitFeature":true,"limits":"Native window/renderer initialization only; OS assistive technology and full pane acceptance untested."})
            );
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ui.ctx().request_repaint();
    }
}
fn ui() -> Result<(), Box<dyn Error>> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_visible(false)
            .with_inner_size([640.0, 480.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "DevWorks disposable probe",
        options,
        Box::new(|_| Ok(Box::<Probe>::default())),
    )
    .map_err(|e| e.to_string().into())
}

#[cfg(target_os = "macos")]
fn secret() -> Result<(), Box<dyn Error>> {
    use security_framework::{os::macos::keychain::SecKeychain, passwords::*};
    // Never prompt or enumerate/read existing credentials. The service is unique.
    let _interaction = SecKeychain::disable_user_interaction()?;
    let service = format!(
        "devworks.disposable.architecture-probe.{}",
        std::process::id()
    );
    let account = "fixture-only";
    let result = (|| -> Result<(), Box<dyn Error>> {
        set_generic_password(&service, account, b"synthetic-v1")?;
        assert_eq!(
            generic_password(PasswordOptions::new_generic_password(&service, account))?,
            b"synthetic-v1"
        );
        set_generic_password(&service, account, b"synthetic-v2")?;
        assert_eq!(
            generic_password(PasswordOptions::new_generic_password(&service, account))?,
            b"synthetic-v2"
        );
        delete_generic_password(&service, account)?;
        assert!(
            generic_password(PasswordOptions::new_generic_password(&service, account)).is_err()
        );
        Ok(())
    })();
    // Attempt cleanup even if an assertion/API operation failed.
    let cleanup = delete_generic_password(&service, account);
    if let Err(e) = result {
        println!(
            "{}",
            json!({"passed":false,"error":e.to_string(),"cleanupAbsentOrDeleted":cleanup.is_ok() || cleanup.as_ref().err().is_some_and(|e|e.code()==-25300)})
        );
        return Err(e);
    }
    println!(
        "{}",
        json!({"passed":true,"checks":["fixture_write_read","fixture_rotation","fixture_delete","deleted_unavailable"],"existingCredentialsRead":false,"userInteraction":false,"limits":"Current unsigned probe identity/current user only; signed app ACL, locked store and other OS targets remain acceptance."})
    );
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn secret() -> Result<(), Box<dyn Error>> {
    Err("macOS-only experiment".into())
}

#[cfg(unix)]
fn worker() -> Result<(), Box<dyn Error>> {
    use std::os::unix::process::CommandExt;
    // Parent survives group TERM only long enough to reap its own leaf.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("leaf");
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            Ok(())
        });
    }
    let mut leaf = command.spawn()?;
    println!("{}", leaf.id());
    use std::io::Write;
    std::io::stdout().flush()?;
    let status = leaf.wait()?;
    if status.success() {
        return Err("leaf was not cancelled".into());
    }
    Ok(())
}
#[cfg(unix)]
fn process() -> Result<(), Box<dyn Error>> {
    use std::{
        io::{BufRead, BufReader},
        os::unix::process::CommandExt,
    };
    let mut child = Command::new(std::env::current_exe()?)
        .arg("worker")
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let pid = child.id() as i32;
    let result = (|| -> Result<serde_json::Value, Box<dyn Error>> {
        let mut line = String::new();
        BufReader::new(child.stdout.take().ok_or("no child pipe")?).read_line(&mut line)?;
        let leaf: i32 = line.trim().parse()?;
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        assert_eq!(unsafe { libc::getpgid(leaf) }, pid);
        if unsafe { libc::kill(-pid, libc::SIGTERM) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let start = std::time::Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                assert!(status.success());
                break;
            }
            if start.elapsed() > Duration::from_secs(5) {
                return Err("owned group did not stop".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(unsafe { libc::kill(leaf, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        Ok(
            json!({"passed":true,"checks":["new_owned_group","descendant_inherits_group","group_cancellation","direct_child_wait","descendant_reaped"],"limits":"Cooperative fixture descendants only; setsid/escaped process, PID reuse, browser and arbitrary toolchain containment not established."}),
        )
    })();
    if result.is_err() {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = child.wait();
    }
    println!("{}", result?);
    Ok(())
}
#[cfg(not(unix))]
fn worker() -> Result<(), Box<dyn Error>> {
    Err("Unix-only experiment".into())
}
#[cfg(not(unix))]
fn process() -> Result<(), Box<dyn Error>> {
    Err("Unix-only experiment".into())
}
