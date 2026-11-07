//! Native owned-process host ([`ProcessHost`]) for the process owner.
//!
//! * **macOS, Linux:** the child leads a new process group (`setpgid(0, 0)` before exec). Tree
//!   membership is read from the process table (`proc_listpgrppids` on macOS, `/proc` on
//!   Linux), excluding zombies. The root is collected only once no other member is alive, so
//!   its pid — and with it the group ID — cannot be reused while the host may still signal the
//!   group; after that the group is never signalled again. A cooperative stop is SIGTERM to the
//!   group, the forced stop SIGKILL. The birth token is the process start time
//!   (`proc_pidinfo` / `/proc/<pid>/stat`), so recovery never trusts a pid alone.
//! * **Windows:** the child is created suspended in a new process group, assigned to a fresh
//!   Job Object configured kill-on-close without breakaway, and only then resumed, so every
//!   descendant it starts belongs to the job. A cooperative stop is CTRL_BREAK to its process
//!   group (only deliverable when it shares a console); the forced stop is `TerminateJobObject`.
//!   The job is empty when its active process count is zero. Closing the job handle (including
//!   when DevWorks itself dies) kills what remains. The birth token is the creation time.
//!
//! A process group is not a sandbox: a descendant that calls `setsid`/`setpgid` leaves it and is
//! not tracked. Nested jobs need Windows 8 or later; a failed assignment kills the suspended
//! child and refuses the launch.
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use devworks_core::ports::process::{
    NativeIdentity, Probe, ProcessError, ProcessHost, ProcessSpec, ProcessTree,
};

/// The native process host of this platform.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativeProcessHost;

impl ProcessHost for NativeProcessHost {
    fn platform(&self) -> &'static str {
        if cfg!(windows) { "windows" } else { "unix" }
    }
    fn environment(&self) -> Vec<(String, String)> {
        std::env::vars_os()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect()
    }
    fn spawn(&self, spec: &ProcessSpec) -> Result<Box<dyn ProcessTree>, ProcessError> {
        if spec.pipe_stdin && spec.stdin.is_some() {
            return Err(ProcessError::Spawn(
                "stdin bytes and an interactive stdin pipe are mutually exclusive".into(),
            ));
        }
        #[cfg(unix)]
        {
            unix::spawn(spec)
        }
        #[cfg(windows)]
        {
            windows::spawn(spec)
        }
    }
    fn current_identity(&self) -> Option<NativeIdentity> {
        #[cfg(unix)]
        {
            unix::current_identity()
        }
        #[cfg(windows)]
        {
            windows::current_identity()
        }
    }
    fn probe(&self, identity: &NativeIdentity) -> Probe {
        #[cfg(unix)]
        {
            unix::probe(identity)
        }
        #[cfg(windows)]
        {
            windows::probe(identity)
        }
    }
    fn terminate(&self, identity: &NativeIdentity) -> Probe {
        #[cfg(unix)]
        {
            unix::terminate(identity)
        }
        #[cfg(windows)]
        {
            windows::terminate(identity)
        }
    }
}

/// Writes the spec's stdin bytes on a thread and closes the pipe, so a child reading requests
/// never blocks the launcher and a child that exits early only ends the write (broken pipe).
fn feed_stdin(child: &mut std::process::Child, spec: &ProcessSpec) {
    if let Some(bytes) = spec.stdin.clone()
        && let Some(mut pipe) = child.stdin.take()
    {
        std::thread::spawn(move || {
            use std::io::Write;
            let _ = pipe.write_all(&bytes);
        });
    }
}
