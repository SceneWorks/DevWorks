//! Native idle-sleep prevention ([`PowerPort`], POWER-01/02).
//!
//! * **macOS:** an owned `caffeinate -i -w <app pid>` child. `-i` creates the
//!   PreventUserIdleSystemSleep assertion (display sleep stays allowed) and `-w` ends it when
//!   DevWorks exits, even after a crash. Chosen over linking IOKit's `IOPMAssertionCreateWithName`
//!   because it is the system's own wrapper of that assertion, needs no framework FFI, and its
//!   lifetime is bounded by the app's process both ways (owned process group, `-w`).
//! * **Linux:** an owned `systemd-inhibit --what=idle --mode=block` child holding logind's idle
//!   inhibitor for `tail --pid=<app pid>`, so it also ends with DevWorks. It is unavailable
//!   without logind (the child exits at once). Whether a desktop environment's own idle-suspend
//!   policy honors the logind idle inhibitor varies; that is platform qualification evidence.
//! * **Windows:** `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)` on a dedicated
//!   thread (the state belongs to the thread), without `ES_DISPLAY_REQUIRED`, cleared on release;
//!   the OS drops it when the process ends.
//!
//! None of these prevents explicit sleep, lid close, shutdown or every OS power policy.
use devworks_core::ports::process::{PowerAssertion, PowerPort, PowerRequest};
#[cfg(unix)]
use devworks_core::ports::process::{ProcessHost, ProcessSpec, ProcessTree};

/// The native power facility of this platform.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativePower;

impl PowerPort for NativePower {
    fn prevent_idle_sleep(&self) -> Result<Box<dyn PowerAssertion>, String> {
        imp::prevent_idle_sleep()
    }
}

/// A helper process that holds the request while it runs.
#[cfg(unix)]
struct Helper {
    tree: Box<dyn ProcessTree>,
    mechanism: &'static str,
}
#[cfg(unix)]
impl PowerAssertion for Helper {
    fn status(&mut self) -> PowerRequest {
        match self.tree.poll() {
            Ok(s) if !s.empty => PowerRequest::Effective {
                mechanism: self.mechanism,
            },
            Ok(s) => PowerRequest::Unavailable(format!(
                "{} exited{}",
                self.mechanism,
                s.exit_code
                    .map_or(String::new(), |c| format!(" with status {c}"))
            )),
            Err(e) => PowerRequest::Unavailable(e.to_string()),
        }
    }
}
// Dropping the helper drops its tree, which kills the owned process group.

#[cfg(unix)]
fn helper(argv: &[String], mechanism: &'static str) -> Result<Box<dyn PowerAssertion>, String> {
    let spec = ProcessSpec::new(argv).ok_or("empty helper command")?;
    let tree = crate::process::NativeProcessHost
        .spawn(&spec)
        .map_err(|e| format!("{mechanism} is not available: {e}"))?;
    let mut helper = Helper { tree, mechanism };
    // A helper that cannot hold the request exits at once (for example without logind).
    std::thread::sleep(std::time::Duration::from_millis(150));
    match helper.status() {
        PowerRequest::Effective { .. } => Ok(Box::new(helper)),
        PowerRequest::Unavailable(e) => Err(e),
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    pub fn prevent_idle_sleep() -> Result<Box<dyn PowerAssertion>, String> {
        let argv = [
            "/usr/bin/caffeinate".to_owned(),
            "-i".to_owned(),
            "-w".to_owned(),
            std::process::id().to_string(),
        ];
        helper(&argv, "caffeinate -i (PreventUserIdleSystemSleep)")
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod imp {
    use super::*;
    pub fn prevent_idle_sleep() -> Result<Box<dyn PowerAssertion>, String> {
        let argv = [
            "systemd-inhibit",
            "--what=idle",
            "--mode=block",
            "--who=DevWorks",
            "--why=Keep the computer awake while DevWorks is open",
            "tail",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .chain([
            format!("--pid={}", std::process::id()),
            "-f".to_owned(),
            "/dev/null".to_owned(),
        ])
        .collect::<Vec<_>>();
        helper(&argv, "logind idle inhibitor (systemd-inhibit)")
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::sync::mpsc;
    use windows_sys::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };

    const MECHANISM: &str = "SetThreadExecutionState (ES_SYSTEM_REQUIRED)";

    struct Held {
        release: Option<mpsc::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl PowerAssertion for Held {
        fn status(&mut self) -> PowerRequest {
            match &self.thread {
                Some(t) if !t.is_finished() => PowerRequest::Effective {
                    mechanism: MECHANISM,
                },
                _ => PowerRequest::Unavailable("the request thread ended".into()),
            }
        }
    }
    impl Drop for Held {
        fn drop(&mut self) {
            drop(self.release.take());
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    pub fn prevent_idle_sleep() -> Result<Box<dyn PowerAssertion>, String> {
        let (release, wait) = mpsc::channel::<()>();
        let (ready, started) = mpsc::channel::<bool>();
        let thread = std::thread::Builder::new()
            .name("devworks-keep-awake".into())
            .spawn(move || {
                // SAFETY: sets this thread's execution state; cleared before the thread ends.
                let ok =
                    unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) } != 0;
                let _ = ready.send(ok);
                if ok {
                    // Held until released (the sender drops).
                    let _ = wait.recv();
                    unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
                }
            })
            .map_err(|e| e.to_string())?;
        match started.recv() {
            Ok(true) => Ok(Box::new(Held {
                release: Some(release),
                thread: Some(thread),
            })),
            _ => {
                let _ = thread.join();
                Err("SetThreadExecutionState refused the request".into())
            }
        }
    }
}
