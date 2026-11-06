//! Owned process groups on macOS and Linux.
use devworks_core::ports::process::ControlChannel;
use devworks_core::ports::process::{
    NativeIdentity, Probe, ProcessError, ProcessSpec, ProcessTree, TreeStatus,
};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Birth token recorded when the start time cannot be read (for example a child that already
/// exited); recovery treats it as unverifiable.
const UNAVAILABLE: &str = "unavailable";
/// Probe detail of a vanished root whose process group still has live members.
const LINGERING: &str = "the recorded root is gone but members of its process group remain";
/// How many times a forced stop re-signals a group that still has live members.
const KILL_ROUNDS: usize = 25;

fn unprivileged_child_policy() -> std::io::Result<()> {
    // SAFETY: credential queries have no side effects or allocation.
    if unsafe { libc::getuid() == 0 || libc::geteuid() == 0 } {
        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
    }
    #[cfg(target_os = "macos")]
    // SAFETY: read-only query also refuses saved/set-id credential contexts.
    if unsafe { libc::issetugid() } != 0 {
        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
    }
    #[cfg(target_os = "linux")]
    {
        let (mut real, mut effective, mut saved) = (0, 0, 0);
        // SAFETY: pointers refer to initialized uid_t records on this thread's stack.
        if unsafe { libc::getresuid(&mut real, &mut effective, &mut saved) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if real == 0 || effective == 0 || saved == 0 {
            return Err(std::io::Error::from_raw_os_error(libc::EPERM));
        }
        // Linux capability ABI v3 has two 32-bit words. Reject retained capabilities:
        // SYS_ADMIN/RESOURCE directly bypass NPROC, while SETUID can regain exempt UID0.
        // NO_NEW_PRIVS below additionally prevents gains from the executed file.
        #[repr(C)]
        struct Header {
            version: u32,
            pid: i32,
        }
        #[repr(C)]
        #[derive(Clone, Copy, Default)]
        struct Data {
            effective: u32,
            permitted: u32,
            inheritable: u32,
        }
        let mut header = Header {
            version: 0x2008_0522,
            pid: 0,
        };
        let mut data = [Data::default(); 2];
        // SAFETY: SYS_capget receives correctly sized initialized ABI records for this thread.
        if unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if data
            .iter()
            .any(|d| d.effective | d.permitted | d.inheritable != 0)
        {
            return Err(std::io::Error::from_raw_os_error(libc::EPERM));
        }
    }
    Ok(())
}

fn establish_child_policy() -> std::io::Result<()> {
    unprivileged_child_policy()?;
    #[cfg(target_os = "linux")]
    // SAFETY: this child-only syscall prevents exec from gaining setuid/file capabilities.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: only this forked child is changed, with a correctly sized resource record.
    if unsafe { libc::setrlimit(libc::RLIMIT_NPROC, &limit) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut observed = limit;
    // SAFETY: the record is writable and initialized.
    if unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &mut observed) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if observed.rlim_cur != 0 || observed.rlim_max != 0 {
        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
    }
    Ok(())
}

fn policy_program(spec: &ProcessSpec) -> std::io::Result<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let cwd = spec.cwd.clone().map_or_else(std::env::current_dir, Ok)?;
    let input = std::path::Path::new(&spec.program);
    let path = if input.is_absolute() {
        input.to_path_buf()
    } else if input.components().count() > 1 {
        cwd.join(input)
    } else {
        let configured = spec
            .env
            .iter()
            .rev()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| std::ffi::OsString::from(value));
        let path = configured
            .or_else(|| {
                (!spec.clear_env)
                    .then(|| std::env::var_os("PATH"))
                    .flatten()
            })
            .unwrap_or_else(|| "/usr/bin:/bin".into());
        std::env::split_paths(&path)
            .map(|base| {
                if base.is_absolute() {
                    base.join(input)
                } else {
                    cwd.join(base).join(input)
                }
            })
            .find(|path| {
                std::fs::metadata(path)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::ENOENT))?
    }
    .canonicalize()?;
    let metadata = std::fs::metadata(&path)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o6000 != 0 {
        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
    }
    Ok(path)
}

pub(super) fn spawn(spec: &ProcessSpec) -> Result<Box<dyn ProcessTree>, ProcessError> {
    if spec.no_child_processes {
        unprivileged_child_policy()
            .map_err(|e| ProcessError::EnforcementUnavailable(e.to_string()))?;
    }
    let program = if spec.no_child_processes {
        policy_program(spec).map_err(|e| ProcessError::EnforcementUnavailable(e.to_string()))?
    } else {
        std::path::PathBuf::from(&spec.program)
    };
    let mut cmd = Command::new(program);
    if spec.no_child_processes {
        // SAFETY: the child hook uses only credential/resource kernel calls and initialized
        // stack records; no locks, heap allocation or parent resource limits are changed.
        unsafe {
            cmd.pre_exec(establish_child_policy);
        }
    }
    cmd.args(&spec.args);
    if spec.clear_env {
        cmd.env_clear();
    }
    cmd.envs(spec.env.iter().map(|(k, v)| (k, v)));
    if let Some(dir) = &spec.cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(if spec.pipe_stdin || spec.stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let out = || {
        if spec.capture_output {
            Stdio::piped()
        } else {
            Stdio::null()
        }
    };
    cmd.stdout(out()).stderr(out());
    // The child leads a new process group, so the whole tree is addressable as one.
    cmd.process_group(0);
    // A private control channel: the child's ends become its fds 3 (read) and 4 (write); the
    // host keeps the other ends, close-on-exec, so no other child inherits them.
    let mut control = None;
    if spec.control_pipe.is_some() {
        let (child_read, host_write) = pipe().map_err(ProcessError::Spawn)?;
        let (host_read, child_write) = pipe().map_err(ProcessError::Spawn)?;
        // Above 4, so placing them on 3 and 4 never overwrites the other.
        let child_read = above(child_read).map_err(ProcessError::Spawn)?;
        let child_write = above(child_write).map_err(ProcessError::Spawn)?;
        let (r, w) = (child_read.as_raw_fd(), child_write.as_raw_fd());
        // SAFETY: dup2 is async-signal-safe; it runs in the forked child before exec and
        // clears close-on-exec on the duplicates only.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(r, 3) < 0 || libc::dup2(w, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        control = Some((child_read, child_write, host_write, host_read));
    }
    let spawned = cmd.spawn();
    let control: Option<ControlChannel> = control.map(|(cr, cw, hw, hr)| {
        // The child's ends close here: end of file reaches the host when the child exits.
        drop((cr, cw));
        (
            Box::new(std::fs::File::from(hw)) as Box<dyn std::io::Write + Send>,
            Box::new(std::fs::File::from(hr)) as Box<dyn Read + Send>,
        )
    });
    let mut child = spawned.map_err(|e| {
        if spec.no_child_processes {
            ProcessError::EnforcementUnavailable(e.to_string())
        } else {
            ProcessError::Spawn(e.to_string())
        }
    })?;
    super::feed_stdin(&mut child, spec);
    let pid = child.id();
    let birth = start_time(pid as i32, true).unwrap_or_else(|| UNAVAILABLE.into());
    let input = child
        .stdin
        .take()
        .map(|i| Box::new(i) as Box<dyn Write + Send>);
    let output = match (child.stdout.take(), child.stderr.take()) {
        (Some(o), Some(e)) => Some((
            Box::new(o) as Box<dyn Read + Send>,
            Box::new(e) as Box<dyn Read + Send>,
        )),
        _ => None,
    };
    Ok(Box::new(UnixTree {
        child,
        identity: NativeIdentity {
            pid,
            birth,
            group: Some(i64::from(pid)),
        },
        empty: false,
        exit_code: None,
        output,
        control,
        input,
    }))
}

/// A close-on-exec pipe: (read end, write end).
fn pipe() -> Result<(OwnedFd, OwnedFd), String> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: a two-element array for the pipe's descriptors.
    #[cfg(target_os = "linux")]
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(target_os = "linux"))]
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(format!("control pipe: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: both descriptors were just created and are owned here.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    #[cfg(not(target_os = "linux"))]
    for fd in [&r, &w] {
        // SAFETY: setting close-on-exec on a descriptor this function owns.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(format!("control pipe: {}", std::io::Error::last_os_error()));
        }
    }
    Ok((r, w))
}

/// A close-on-exec duplicate of `fd` numbered above 4.
fn above(fd: OwnedFd) -> Result<OwnedFd, String> {
    // SAFETY: duplicating a descriptor this function owns.
    let n = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 5) };
    if n < 0 {
        return Err(format!("control pipe: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: `n` was just created and is owned by the result.
    Ok(unsafe { OwnedFd::from_raw_fd(n) })
}

struct UnixTree {
    /// Not collected until the rest of the group is gone, so the group ID stays reserved.
    child: Child,
    identity: NativeIdentity,
    empty: bool,
    exit_code: Option<i32>,
    output: Option<(Box<dyn Read + Send>, Box<dyn Read + Send>)>,
    control: Option<ControlChannel>,
    input: Option<Box<dyn Write + Send>>,
}

impl UnixTree {
    fn pgid(&self) -> libc::pid_t {
        self.identity.pid as libc::pid_t
    }
    fn signal(&mut self, signal: libc::c_int) -> Result<(), ProcessError> {
        if self.empty {
            return Ok(());
        }
        // SAFETY: plain signal syscalls on a group and a pid this tree still reserves (its root
        // is not collected yet, so neither ID can have been reused).
        let group = unsafe { libc::killpg(self.pgid(), signal) };
        let group_err = std::io::Error::last_os_error();
        // The root itself, in case it left its group.
        unsafe { libc::kill(self.identity.pid as libc::pid_t, signal) };
        // ESRCH: nothing left. macOS reports EPERM for a group whose remaining members are all
        // exiting or zombies; that is a stopped group too.
        if group != 0
            && group_err.raw_os_error() != Some(libc::ESRCH)
            && !members(self.pgid()).is_ok_and(|m| m.is_empty())
        {
            return Err(ProcessError::Control(format!(
                "signal the process group: {group_err}"
            )));
        }
        Ok(())
    }
}

impl ProcessTree for UnixTree {
    fn identity(&self) -> &NativeIdentity {
        &self.identity
    }
    fn request_stop(&mut self) -> Result<bool, ProcessError> {
        self.signal(libc::SIGTERM)?;
        Ok(true)
    }
    fn kill(&mut self) -> Result<(), ProcessError> {
        // A member forking while the group is signalled can create a child the signal missed
        // (observed on macOS), so the group is signalled again until no live member remains,
        // within a short bound. The root stays uncollected throughout, so the group ID is ours.
        for _ in 0..KILL_ROUNDS {
            self.signal(libc::SIGKILL)?;
            if self.empty || members(self.pgid())?.is_empty() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
    fn poll(&mut self) -> Result<TreeStatus, ProcessError> {
        if !self.empty {
            let live = members(self.pgid())?;
            if live.is_empty() {
                // Every member is gone or a zombie: collect the root (it may have left the
                // group and still run, in which case the tree is not empty).
                match self.child.try_wait() {
                    Ok(Some(status)) => {
                        self.empty = true;
                        self.exit_code = status.code();
                    }
                    Ok(None) => {}
                    Err(e) => return Err(ProcessError::Control(format!("collect the root: {e}"))),
                }
            }
        }
        Ok(TreeStatus {
            empty: self.empty,
            exit_code: self.exit_code,
        })
    }
    fn take_output(&mut self) -> Option<(Box<dyn Read + Send>, Box<dyn Read + Send>)> {
        self.output.take()
    }
    fn root_exited(&mut self) -> Result<bool, ProcessError> {
        if self.empty {
            return Ok(true);
        }
        // The root is not collected while the tree lives, so its pid is still ours: it has
        // exited once it is a zombie (or gone).
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            Ok(start_time(self.identity.pid as libc::pid_t, false).is_none())
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            self.poll().map(|s| s.empty)
        }
    }
    fn take_control(&mut self) -> Option<ControlChannel> {
        self.control.take()
    }
    fn take_input(&mut self) -> Option<Box<dyn Write + Send>> {
        self.input.take()
    }
}

impl Drop for UnixTree {
    fn drop(&mut self) {
        if self.empty {
            return;
        }
        let _ = self.kill();
        // Collect the root within a short bound so no zombie lingers; a tree that does not
        // stop stays the owner's recorded uncertainty.
        let until = Instant::now() + Duration::from_millis(500);
        while Instant::now() < until {
            if let Ok(TreeStatus { empty: true, .. }) = self.poll() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

pub(super) fn probe(identity: &NativeIdentity) -> Probe {
    let pid = identity.pid as libc::pid_t;
    if identity.birth == UNAVAILABLE {
        return Probe::Unknown("the recorded start time was unavailable".into());
    }
    match start_time(pid, false) {
        Some(b) if b == identity.birth => Probe::Alive,
        // The recorded root is gone (or its pid now belongs to another process).
        _ => match identity.group.map(|g| members(g as libc::pid_t)) {
            None => Probe::Gone,
            Some(Ok(m)) if m.is_empty() => Probe::Gone,
            Some(Ok(_)) => Probe::Unknown(LINGERING.into()),
            Some(Err(e)) => Probe::Unknown(e.to_string()),
        },
    }
}

pub(super) fn terminate(identity: &NativeIdentity) -> Probe {
    if probe(identity) != Probe::Alive {
        return probe(identity);
    }
    let pid = identity.pid as libc::pid_t;
    let group = identity.group.map_or(pid, |g| g as libc::pid_t);
    // The group is signalled again while it has live members: a member forking as a signal is
    // delivered can create a child the signal missed. The group ID cannot be reused while any
    // member lives, and signalling stops once none does.
    for round in 0..KILL_ROUNDS {
        // SAFETY: the root was verified alive with its recorded start time, so the group it
        // leads is the recorded one; afterwards it persists while any member lives.
        unsafe {
            libc::killpg(group, libc::SIGKILL);
            if round == 0 {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
        if members(group).is_ok_and(|m| m.is_empty()) {
            break;
        }
    }
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let p = probe(identity);
        if p != Probe::Alive && p != Probe::Unknown(LINGERING.into()) || Instant::now() >= until {
            return p;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Live (non-zombie) members of a process group.
#[cfg(target_os = "macos")]
fn members(pgid: libc::pid_t) -> Result<Vec<libc::pid_t>, ProcessError> {
    let mut capacity = 64usize;
    loop {
        let mut buf = vec![0 as libc::pid_t; capacity];
        let bytes = (capacity * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the buffer holds `capacity` pids and its size is passed in bytes.
        let n = unsafe { libc::proc_listpgrppids(pgid, buf.as_mut_ptr().cast(), bytes) };
        if n < 0 {
            return Err(ProcessError::Control(format!(
                "list the process group: {}",
                std::io::Error::last_os_error()
            )));
        }
        let n = n as usize;
        if n >= capacity {
            capacity *= 4;
            continue;
        }
        buf.truncate(n);
        return Ok(buf
            .into_iter()
            .filter(|p| *p > 0 && bsd_info(*p).is_some_and(|i| i.pbi_status != libc::SZOMB))
            .collect());
    }
}
#[cfg(target_os = "macos")]
fn bsd_info(pid: libc::pid_t) -> Option<libc::proc_bsdinfo> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a properly sized, writable proc_bsdinfo.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (n == size).then_some(info)
}
#[cfg(target_os = "macos")]
fn start_time(pid: libc::pid_t, allow_zombie: bool) -> Option<String> {
    let info = bsd_info(pid)?;
    if !allow_zombie && info.pbi_status == libc::SZOMB {
        return None;
    }
    Some(format!(
        "{}.{:06}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    ))
}

/// `(state, pgrp, starttime)` of `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn stat(pid: libc::pid_t) -> Option<(char, i64, String)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is parenthesized and may contain anything: fields follow the last ')'.
    let rest = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // fields[0] is the state (field 3), fields[2] the process group (field 5) and fields[19]
    // the start time in clock ticks since boot (field 22).
    let state = fields.first()?.chars().next()?;
    let pgrp = fields.get(2)?.parse().ok()?;
    let start = (*fields.get(19)?).to_owned();
    Some((state, pgrp, start))
}
#[cfg(target_os = "linux")]
fn members(pgid: libc::pid_t) -> Result<Vec<libc::pid_t>, ProcessError> {
    let dir = std::fs::read_dir("/proc")
        .map_err(|e| ProcessError::Control(format!("read the process table: {e}")))?;
    Ok(dir
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str()?.parse::<libc::pid_t>().ok())
        .filter(|pid| {
            stat(*pid).is_some_and(|(state, pgrp, _)| {
                pgrp == i64::from(pgid) && !matches!(state, 'Z' | 'X' | 'x')
            })
        })
        .collect())
}
#[cfg(target_os = "linux")]
fn start_time(pid: libc::pid_t, allow_zombie: bool) -> Option<String> {
    let (state, _, start) = stat(pid)?;
    if !allow_zombie && matches!(state, 'Z' | 'X' | 'x') {
        return None;
    }
    // Boot-relative ticks: include the boot identity so a token never matches across reboots.
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default();
    Some(format!("{}:{start}", boot.trim()))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn members(_: libc::pid_t) -> Result<Vec<libc::pid_t>, ProcessError> {
    Err(ProcessError::Control(
        "process-group membership is not qualified on this platform".into(),
    ))
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn start_time(_: libc::pid_t, _: bool) -> Option<String> {
    None
}
