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
    if let Some(arg0) = &spec.arg0 {
        cmd.arg0(arg0);
    }
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
    let merged_output: Option<Box<dyn Read + Send>> = if spec.merge_output {
        if !spec.capture_output {
            return Err(ProcessError::Spawn("merged output requires capture".into()));
        }
        let (reader, writer) = pipe().map_err(ProcessError::Spawn)?;
        let writer = std::fs::File::from(writer);
        let stderr = writer
            .try_clone()
            .map_err(|error| ProcessError::Spawn(error.to_string()))?;
        cmd.stdout(Stdio::from(writer)).stderr(Stdio::from(stderr));
        Some(Box::new(std::fs::File::from(reader)))
    } else {
        cmd.stdout(out()).stderr(out());
        None
    };
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
    let output = merged_output
        .map(|read| (read, Box::new(std::io::empty()) as Box<dyn Read + Send>))
        .or_else(|| match (child.stdout.take(), child.stderr.take()) {
            (Some(o), Some(e)) => Some((
                Box::new(o) as Box<dyn Read + Send>,
                Box::new(e) as Box<dyn Read + Send>,
            )),
            _ => None,
        });
    Ok(Box::new(UnixTree {
        child,
        identity: NativeIdentity {
            pid,
            birth,
            group: Some(i64::from(pid)),
        },
        empty: false,
        exit_code: None,
        signal: None,
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
    signal: Option<i32>,
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
                        self.signal = std::os::unix::process::ExitStatusExt::signal(&status);
                    }
                    Ok(None) => {}
                    Err(e) => return Err(ProcessError::Control(format!("collect the root: {e}"))),
                }
            }
        }
        Ok(TreeStatus {
            empty: self.empty,
            exit_code: self.exit_code,
            signal: self.signal,
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
            observed_start_time(self.identity.pid as libc::pid_t, false)
                .map(|birth| birth.is_none())
                .map_err(ProcessError::Control)
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

pub(super) fn current_identity() -> Option<NativeIdentity> {
    let pid = std::process::id();
    let birth = start_time(pid as libc::pid_t, false)?;
    Some(NativeIdentity {
        pid,
        birth,
        // The host is not one of our launched group leaders. Its liveness is process-only.
        group: None,
    })
}

pub(super) fn probe(identity: &NativeIdentity) -> Probe {
    probe_birth(
        identity,
        observed_start_time(identity.pid as libc::pid_t, false),
    )
}

fn probe_birth(identity: &NativeIdentity, birth: Result<Option<String>, String>) -> Probe {
    if !valid_recorded_birth(&identity.birth) {
        return Probe::Unknown("the recorded start time was unavailable".into());
    }
    match birth {
        Ok(Some(b)) if !valid_recorded_birth(&b) => {
            Probe::Unknown("the process birth token was malformed".into())
        }
        Ok(Some(b)) if b == identity.birth => Probe::Alive,
        Err(reason) => Probe::Unknown(reason),
        // A confirmed absence, zombie, or known different birth is not this live root.
        Ok(_) => match identity.group.map(|g| members(g as libc::pid_t)) {
            None => Probe::Gone,
            Some(Ok(m)) if m.is_empty() => Probe::Gone,
            Some(Ok(_)) => Probe::Unknown(LINGERING.into()),
            Some(Err(e)) => Probe::Unknown(e.to_string()),
        },
    }
}

/// A missing proc entry is not enough when the process table itself may be unavailable.
/// Signal zero checks OS existence without delivering a signal or changing the process.
fn confirm_absent(pid: libc::pid_t) -> Result<(), String> {
    if pid <= 0 {
        return Err("invalid process identity".into());
    }
    // SAFETY: a positive PID and signal zero only query process existence/permission.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Err("the process exists but its birth token could not be read".into());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(format!("could not establish process absence: {error}"))
    }
}

fn process_read_failure<T>(pid: libc::pid_t, error: std::io::Error) -> Result<Option<T>, String> {
    classify_read_failure(error, || confirm_absent(pid))
}

fn classify_read_failure<T>(
    error: std::io::Error,
    absent: impl FnOnce() -> Result<(), String>,
) -> Result<Option<T>, String> {
    if error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH) {
        absent()?;
        Ok(None)
    } else {
        Err(format!("read process birth information: {error}"))
    }
}

fn start_time(pid: libc::pid_t, allow_zombie: bool) -> Option<String> {
    observed_start_time(pid, allow_zombie)
        .ok()
        .flatten()
        .filter(|birth| valid_recorded_birth(birth))
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
        let mut live = Vec::new();
        for pid in buf.into_iter().filter(|pid| *pid > 0) {
            match bsd_info(pid).map_err(ProcessError::Control)? {
                Some(info) if info.pbi_status != libc::SZOMB => live.push(pid),
                _ => {}
            }
        }
        return Ok(live);
    }
}
#[cfg(target_os = "macos")]
fn bsd_info(pid: libc::pid_t) -> Result<Option<libc::proc_bsdinfo>, String> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a properly sized, writable proc_bsdinfo.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            // Include zombies so their known exit is distinct from an unreadable live PID.
            1,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if n == size {
        return Ok(Some(info));
    }
    let error = std::io::Error::last_os_error();
    if n <= 0 {
        return process_read_failure(pid, error);
    }
    Err("incomplete process birth information".into())
}
#[cfg(target_os = "macos")]
fn valid_recorded_birth(birth: &str) -> bool {
    let Some((seconds, micros)) = birth.split_once('.') else {
        return false;
    };
    seconds
        .parse::<u64>()
        .is_ok_and(|n| n.to_string() == seconds)
        && micros.len() == 6
        && micros.bytes().all(|b| b.is_ascii_digit())
}
#[cfg(target_os = "linux")]
fn valid_recorded_birth(birth: &str) -> bool {
    valid_linux_birth(birth)
}
#[cfg(any(target_os = "linux", test))]
fn valid_linux_birth(birth: &str) -> bool {
    let Some((boot, ticks)) = birth.split_once(':') else {
        return false;
    };
    checked_boot_identity(boot).is_ok_and(|canonical| canonical == boot)
        && ticks.parse::<u64>().is_ok_and(|n| n.to_string() == ticks)
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn valid_recorded_birth(_: &str) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn observed_start_time(pid: libc::pid_t, allow_zombie: bool) -> Result<Option<String>, String> {
    let Some(info) = bsd_info(pid)? else {
        return Ok(None);
    };
    if !allow_zombie && info.pbi_status == libc::SZOMB {
        return Ok(None);
    }
    Ok(Some(format!(
        "{}.{:06}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    )))
}

/// `(state, pgrp, starttime)` of `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn stat(pid: libc::pid_t) -> Result<Option<(char, i64, String)>, String> {
    let text = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(value) => value,
        Err(error) => return process_read_failure(pid, error),
    };
    parse_stat(&text).map(Some)
}
#[cfg(any(target_os = "linux", test))]
fn parse_stat(text: &str) -> Result<(char, i64, String), String> {
    let invalid = || "malformed process birth information".to_owned();
    // The command name is parenthesized and may contain anything: fields follow the last ')'.
    let rest = &text[text.rfind(')').ok_or_else(invalid)? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let state = fields
        .first()
        .and_then(|s| s.chars().next())
        .ok_or_else(invalid)?;
    let pgrp = fields
        .get(2)
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    let start = fields.get(19).ok_or_else(invalid)?;
    let _: u64 = start.parse().map_err(|_| invalid())?;
    Ok((state, pgrp, (*start).to_owned()))
}
#[cfg(target_os = "linux")]
fn members(pgid: libc::pid_t) -> Result<Vec<libc::pid_t>, ProcessError> {
    let dir = std::fs::read_dir("/proc")
        .map_err(|e| ProcessError::Control(format!("read the process table: {e}")))?;
    let mut live = Vec::new();
    for entry in dir {
        let entry =
            entry.map_err(|e| ProcessError::Control(format!("read the process table: {e}")))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        if let Some((state, group, _)) = stat(pid).map_err(ProcessError::Control)?
            && group == i64::from(pgid)
            && !matches!(state, 'Z' | 'X' | 'x')
        {
            live.push(pid);
        }
    }
    Ok(live)
}
#[cfg(target_os = "linux")]
fn observed_start_time(pid: libc::pid_t, allow_zombie: bool) -> Result<Option<String>, String> {
    let Some((state, _, start)) = stat(pid)? else {
        return Ok(None);
    };
    if !allow_zombie && matches!(state, 'Z' | 'X' | 'x') {
        return Ok(None);
    }
    // Boot-relative ticks need the real kernel boot identity; an unreadable boot file never
    // becomes an empty marker that could later falsely look like a different process.
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|e| format!("read process boot identity: {e}"))?;
    let boot = checked_boot_identity(&boot)?;
    Ok(Some(format!("{boot}:{start}")))
}
#[cfg(any(target_os = "linux", test))]
fn checked_boot_identity(value: &str) -> Result<&str, String> {
    let value = value.trim();
    if value.len() != 36
        || !value.bytes().enumerate().all(|(index, b)| {
            if [8, 13, 18, 23].contains(&index) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        Err("the process boot identity was unavailable or malformed".into())
    } else {
        Ok(value)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn members(_: libc::pid_t) -> Result<Vec<libc::pid_t>, ProcessError> {
    Err(ProcessError::Control(
        "process-group membership is not qualified on this platform".into(),
    ))
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn observed_start_time(_: libc::pid_t, _: bool) -> Result<Option<String>, String> {
    Err("process birth identity is not qualified on this platform".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn unavailable_birth_is_unknown_and_only_confirmed_absence_is_gone() {
        let identity = current_identity().expect("actual host identity");
        let missing = classify_read_failure::<String>(
            Error::new(ErrorKind::NotFound, "missing proc entry"),
            || Ok(()),
        );
        assert_eq!(probe_birth(&identity, missing), Probe::Gone);
        let unreadable = classify_read_failure::<String>(
            Error::new(ErrorKind::PermissionDenied, "denied"),
            || panic!("denied reads cannot establish absence"),
        );
        assert!(matches!(
            probe_birth(&identity, unreadable),
            Probe::Unknown(_)
        ));
        let table_missing = classify_read_failure::<String>(
            Error::new(ErrorKind::NotFound, "proc unavailable"),
            || Err("the process exists".into()),
        );
        assert!(matches!(
            probe_birth(&identity, table_missing),
            Probe::Unknown(_)
        ));
        assert!(matches!(
            probe_birth(&identity, Ok(Some("malformed-birth".into()))),
            Probe::Unknown(_)
        ));
        let mut other = identity.clone();
        let last = other.birth.pop().expect("birth digit");
        other.birth.push(if last == '0' { '1' } else { '0' });
        assert_eq!(probe_birth(&identity, Ok(Some(other.birth))), Probe::Gone);
        assert_eq!(
            probe_birth(&identity, Ok(Some(identity.birth.clone()))),
            Probe::Alive
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn missing_identity_data_of_the_live_host_does_not_prove_absence() {
        let pid = std::process::id() as libc::pid_t;
        assert!(
            process_read_failure::<String>(
                pid,
                Error::new(ErrorKind::NotFound, "birth read failed")
            )
            .is_err()
        );
        assert!(
            process_read_failure::<String>(
                pid,
                Error::new(ErrorKind::PermissionDenied, "birth read denied")
            )
            .is_err()
        );
        assert!(matches!(
            probe(&current_identity().expect("actual host identity")),
            Probe::Alive
        ));
    }

    #[test]
    fn missing_boot_identity_and_malformed_proc_records_are_unavailable() {
        assert!(checked_boot_identity("").is_err());
        assert!(checked_boot_identity("\n").is_err());
        assert!(!valid_linux_birth(":12345"));
        assert!(!valid_linux_birth(
            " 01234567-89ab-cdef-0123-456789abcdef:12345"
        ));
        assert!(!valid_linux_birth(
            "01234567-89ab-cdef-0123-456789abcdef :12345"
        ));
        assert!(!valid_linux_birth("invalid-boot:12345"));
        assert!(!valid_linux_birth(
            "01234567-89ab-cdef-0123-456789abcdef:invalid-ticks"
        ));
        let legacy = NativeIdentity {
            pid: std::process::id(),
            birth: ":12345".into(),
            group: None,
        };
        assert!(matches!(
            probe_birth(
                &legacy,
                observed_start_time(std::process::id() as libc::pid_t, false)
            ),
            Probe::Unknown(_)
        ));
        assert!(parse_stat("malformed process record").is_err());
        assert_eq!(
            checked_boot_identity("01234567-89ab-cdef-0123-456789abcdef\n").unwrap(),
            "01234567-89ab-cdef-0123-456789abcdef"
        );
    }
}
