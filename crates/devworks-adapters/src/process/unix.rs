//! Owned process groups on macOS and Linux.
use devworks_core::ports::process::{
    NativeIdentity, Probe, ProcessError, ProcessSpec, ProcessTree, TreeStatus,
};
use std::io::{Read, Write};
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

pub(super) fn spawn(spec: &ProcessSpec) -> Result<Box<dyn ProcessTree>, ProcessError> {
    let mut cmd = Command::new(&spec.program);
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
    let mut child = cmd
        .spawn()
        .map_err(|e| ProcessError::Spawn(e.to_string()))?;
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
        input,
    }))
}

struct UnixTree {
    /// Not collected until the rest of the group is gone, so the group ID stays reserved.
    child: Child,
    identity: NativeIdentity,
    empty: bool,
    exit_code: Option<i32>,
    output: Option<(Box<dyn Read + Send>, Box<dyn Read + Send>)>,
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
