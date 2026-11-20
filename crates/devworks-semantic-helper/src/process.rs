//! Resource limits and cleanup for a compiler in the helper's own process group.
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MEMORY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const SAMPLE: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug)]
pub struct Failure {
    pub code: &'static str,
    pub stop_group: bool,
}

impl From<&'static str> for Failure {
    fn from(code: &'static str) -> Self {
        Self {
            code,
            stop_group: false,
        }
    }
}

impl Failure {
    fn stop(code: &'static str) -> Self {
        Self {
            code,
            stop_group: true,
        }
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub fn owned_group() -> bool {
    // The DevWorks process owner puts the helper in a new group led by its own PID.
    unsafe { libc::getpgrp() == libc::getpid() }
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub fn owned_group() -> bool {
    false
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) fn stop_group() -> ! {
    if owned_group() {
        // Only the process supervisor's group led by this helper, never the host's group.
        unsafe { libc::killpg(libc::getpid(), libc::SIGKILL) };
    }
    std::process::exit(1);
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub(crate) fn stop_group() -> ! {
    std::process::exit(1);
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn group_members(members: &mut [libc::pid_t]) -> Result<usize, Failure> {
    if !owned_group() {
        return Err("unowned-process-group".into());
    }
    let bytes = std::mem::size_of_val(members)
        .try_into()
        .expect("fixed buffer");
    // SAFETY: the group is the helper's own, and members is a writable PID array.
    let count =
        unsafe { libc::proc_listpgrppids(libc::getpid(), members.as_mut_ptr().cast(), bytes) };
    usize::try_from(count)
        .ok()
        .filter(|count| *count <= members.len())
        .ok_or(Failure::stop("process-observation-failed"))
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn memory() -> Result<u64, Failure> {
    let mut members = [0; 128];
    let count = group_members(&mut members)?;
    if count == 0 || count >= members.len() {
        return Err(Failure::stop("compiler-process-budget"));
    }
    let mut total = 0u64;
    for pid in members[..count].iter().copied().filter(|id| *id > 0) {
        let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
        // SAFETY: proc_pidinfo writes to a buffer sized for PROC_PIDTASKINFO.
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if n == size {
            // SAFETY: the size check confirms the entire value was written.
            total = total.saturating_add(unsafe { info.assume_init() }.pti_resident_size);
        }
    }
    Ok(total)
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn memory() -> Result<u64, Failure> {
    Err("unsupported-semantic-pack-target".into())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn stop_compiler_children(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_millis(800);
    // Leave the helper alive to remove its scratch directory and send the broker outcome.
    let leader = unsafe { libc::getpid() };
    loop {
        let _ = child.try_wait();
        let mut members = [0; 256];
        let Ok(count) = group_members(&mut members) else {
            break;
        };
        let mut found_child = false;
        for pid in members[..count].iter().copied() {
            if pid != leader && pid > 0 && unsafe { libc::getpgid(pid) == leader } {
                found_child = true;
                // SAFETY: this PID was reported in the helper's live owned group and still
                // belongs to that group. The helper itself is excluded.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        if !found_child {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    stop_group();
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn stop_compiler_children(_child: &mut Child) {
    stop_group();
}

fn budget(
    deadline: Instant,
    cancelled: &AtomicBool,
    observe: impl FnOnce() -> Result<u64, Failure>,
) -> Result<(), Failure> {
    if cancelled.load(Ordering::Acquire) {
        return Err(Failure::stop("cancelled"));
    }
    if Instant::now() >= deadline {
        return Err(Failure::stop("compiler-timeout"));
    }
    if observe()? > MEMORY_BYTES {
        return Err(Failure::stop("compiler-memory-budget"));
    }
    Ok(())
}

/// Bound every file a compiler child can write, including its result and diagnostic output.
pub fn limit_file_size(command: &mut Command, bytes: u64) {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: the fork hook makes one async-signal-safe system call with stack data.
        unsafe {
            command.pre_exec(move || {
                let limit = libc::rlimit {
                    rlim_cur: bytes,
                    rlim_max: bytes,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (command, bytes);
}

pub fn run(
    command: &mut Command,
    deadline: Instant,
    cancelled: &AtomicBool,
    failure: &'static str,
) -> Result<(), Failure> {
    budget(deadline, cancelled, memory)?;
    let mut child = command.stdin(Stdio::null()).spawn().map_err(|_| failure)?;
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break Ok(()),
            Ok(Some(_)) | Err(_) => break Err(Failure::stop(failure)),
            Ok(None) => {}
        }
        if let Err(error) = budget(deadline, cancelled, memory) {
            break Err(error);
        }
        std::thread::sleep(SAMPLE);
    };
    stop_compiler_children(&mut child);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resource_check_reports_memory_timeout_and_cancellation_as_faults() {
        let cancel = AtomicBool::new(false);
        let until = Instant::now() + Duration::from_secs(5);
        assert!(budget(until, &cancel, || Ok(MEMORY_BYTES)).is_ok());
        let error = budget(until, &cancel, || Ok(MEMORY_BYTES + 1)).unwrap_err();
        assert_eq!(error.code, "compiler-memory-budget");
        assert!(error.stop_group);
        let expired = budget(Instant::now(), &cancel, || Ok(0)).unwrap_err();
        assert_eq!(expired.code, "compiler-timeout");
        cancel.store(true, Ordering::Release);
        let cancelled = budget(until, &cancel, || Ok(0)).unwrap_err();
        assert_eq!(cancelled.code, "cancelled");
    }
}
