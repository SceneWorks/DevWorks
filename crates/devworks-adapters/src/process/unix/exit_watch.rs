//! Read-only process exit notifications on macOS and Linux.
use super::{observed_start_time, valid_recorded_birth};
use devworks_core::ports::process::{
    ExternalExit, ExternalProcessStatus as Status, ExternalProcessWatch, NativeIdentity,
    ProcessError,
};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub(in crate::process) fn inspect_external(identity: &NativeIdentity) -> Status {
    let Ok(pid) = libc::pid_t::try_from(identity.pid) else {
        return Status::InvalidIdentity;
    };
    if pid <= 0 || !valid_recorded_birth(&identity.birth) {
        return Status::InvalidIdentity;
    }
    match observed_start_time(pid, true) {
        Ok(Some(birth)) if birth != identity.birth => return Status::Different,
        Ok(Some(_)) => {}
        Ok(None) => return Status::Missing,
        Err(_) => return Status::Unavailable,
    }
    match observed_start_time(pid, false) {
        Ok(Some(birth)) if birth == identity.birth => Status::Running,
        Ok(Some(_)) => Status::Different,
        Ok(None) => Status::Exited(ExternalExit::default()),
        Err(_) => Status::Unavailable,
    }
}

pub(in crate::process) fn watch_external(
    identity: &NativeIdentity,
) -> Result<Box<dyn ExternalProcessWatch>, Status> {
    let status = inspect_external(identity);
    if status != Status::Running {
        return Err(status);
    }
    let fd = register(identity.pid);
    // Both Unix registration APIs take a PID. Re-read the birth token after acquiring the
    // registration so a reused PID cannot lend its exit notification to the original process.
    let status = inspect_external(identity);
    match (fd, status) {
        (Ok(fd), Status::Running | Status::Exited(_)) => Ok(fd),
        (_, Status::Running) => Err(Status::Unavailable),
        (_, status) => Err(status),
    }
}

#[cfg(target_os = "macos")]
fn register(pid: u32) -> std::io::Result<Box<dyn ExternalProcessWatch>> {
    // SAFETY: kqueue creates a new descriptor owned by the caller.
    let fd = owned_fd(unsafe { libc::kqueue() })?;
    // SAFETY: this is our queue descriptor.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let change = libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: one valid change, no event output. With no event list this call does not wait.
    if unsafe {
        libc::kevent(
            fd.as_raw_fd(),
            &change,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Box::new(ExitWatch(fd)))
}

#[cfg(target_os = "linux")]
fn register(pid: u32) -> std::io::Result<Box<dyn ExternalProcessWatch>> {
    // SAFETY: pidfd_open takes a positive PID and zero flags. The returned descriptor is owned.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0u32) };
    Ok(Box::new(ExitWatch(owned_fd(raw as libc::c_int)?)))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn register(_: u32) -> std::io::Result<Box<dyn ExternalProcessWatch>> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn owned_fd(raw: libc::c_int) -> std::io::Result<OwnedFd> {
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful registration call returned ownership of this descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct ExitWatch(OwnedFd);

#[cfg(target_os = "macos")]
impl ExternalProcessWatch for ExitWatch {
    fn poll_exit(&mut self) -> Result<Option<ExternalExit>, ProcessError> {
        let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: one writable event, no changes, and a zero timeout for nonblocking polling.
        let count = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                std::ptr::null(),
                0,
                event.as_mut_ptr(),
                1,
                &timeout,
            )
        };
        match count {
            0 => Ok(None),
            1 => {
                // SAFETY: kevent initialized the event because it returned one.
                let event = unsafe { event.assume_init() };
                if event.flags & libc::EV_ERROR != 0 || event.fflags & libc::NOTE_EXIT == 0 {
                    return Err(ProcessError::Control(
                        "unexpected external process event".into(),
                    ));
                }
                Ok(Some(ExternalExit::default()))
            }
            _ => Err(ProcessError::Control(
                std::io::Error::last_os_error().to_string(),
            )),
        }
    }
}

#[cfg(target_os = "linux")]
impl ExternalProcessWatch for ExitWatch {
    fn poll_exit(&mut self) -> Result<Option<ExternalExit>, ProcessError> {
        let mut event = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid descriptor and a zero timeout for nonblocking polling.
        let count = unsafe { libc::poll(&mut event, 1, 0) };
        if count < 0 {
            return Err(ProcessError::Control(
                std::io::Error::last_os_error().to_string(),
            ));
        }
        if event.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(ProcessError::Control(
                "could not poll the external process".into(),
            ));
        }
        Ok((event.revents & (libc::POLLIN | libc::POLLHUP) != 0).then(ExternalExit::default))
    }
}
