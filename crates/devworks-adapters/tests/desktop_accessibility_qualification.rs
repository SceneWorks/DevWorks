//! Explicitly dispatched native GUI qualification. Normal CI does not have a prepared desktop.
#![cfg(any(target_os = "windows", target_os = "linux"))]

use devworks_adapters::desktop::NativeDesktop;
use devworks_core::ports::desktop::{DesktopAvailability, DesktopFault, DesktopPlatform};
use devworks_core::protocol::records::DesktopInput;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Fixture(Child);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "requires a prepared interactive native desktop; run the accessibility qualification workflow"]
fn native_target_action_reaches_disposable_control() {
    let marker =
        std::env::temp_dir().join(format!("devworks-accessibility-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/ci")
        .join(if cfg!(target_os = "windows") {
            "accessibility-windows.ps1"
        } else {
            "accessibility-linux.py"
        });
    let mut command = if cfg!(target_os = "windows") {
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-STA", "-File"]);
        command
    } else {
        Command::new("/usr/bin/python3")
    };
    let _fixture = Fixture(
        command
            .arg(script)
            .env("DEVWORKS_ACCESSIBILITY_MARKER", &marker)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start disposable native window"),
    );
    let platform = NativeDesktop;
    if cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        assert!(matches!(
            platform.capabilities().input,
            DesktopAvailability::Unsupported(_)
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut saw_target = false;
    loop {
        let capability = platform.capabilities().accessibility;
        if matches!(&capability, DesktopAvailability::Available)
            && let Ok(snapshot) = platform.accessibility_snapshot()
            && let Some(node) = snapshot
                .nodes
                .iter()
                .find(|node| node.name == "Qualify Action")
            && let Some(action) = node
                .actions
                .iter()
                .find(|action| *action != "focus" && *action != "setValue")
        {
            saw_target = true;
            let input: DesktopInput = serde_json::from_value(serde_json::json!({
                "kind":"Target", "targetId":node.target_id,
                "snapshotRevision":snapshot.revision(),
                "action":{"kind":"Invoke","action":action}
            }))
            .expect("typed native target action");
            match platform.inject(&input) {
                Ok(()) => break,
                Err(DesktopFault::StaleTarget) => {}
                Err(fault) => panic!("native target action failed: {fault:?}"),
            }
        }
        assert!(
            Instant::now() < deadline,
            "native target was not actionable: saw_target={saw_target}, capability={capability:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "native fixture did not observe the action"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "pressed");
    std::fs::remove_file(marker).expect("remove fixture marker");
}
