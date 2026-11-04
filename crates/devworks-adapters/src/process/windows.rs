//! Owned process trees on Windows: a kill-on-close Job Object per tree, assigned while the
//! child is still suspended.
use devworks_core::ports::process::{
    ControlChannel, NativeIdentity, Probe, ProcessError, ProcessSpec, ProcessTree, TreeStatus,
};
use std::io::Read;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, STILL_ACTIVE,
    SetHandleInformation,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, GetExitCodeProcess, GetProcessTimes, OpenProcess,
    OpenThread, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    ResumeThread, THREAD_SUSPEND_RESUME, TerminateProcess, WaitForSingleObject,
};

fn last_error(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

/// An owned kernel handle, closed once.
struct Owned(HANDLE);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: the handle is owned by this value and closed exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn creation_time(process: HANDLE) -> Option<String> {
    let mut times: [FILETIME; 4] = unsafe { std::mem::zeroed() };
    let [c, e, k, u] = &mut times;
    // SAFETY: four writable FILETIME out-parameters for a valid process handle.
    let ok = unsafe { GetProcessTimes(process, c, e, k, u) };
    (ok != 0).then(|| {
        (u64::from(times[0].dwHighDateTime) << 32 | u64::from(times[0].dwLowDateTime)).to_string()
    })
}

/// Resumes every thread of a process created suspended.
fn resume(pid: u32) -> Result<(), String> {
    // SAFETY: a snapshot of the system's threads; the handle is closed by `Owned`.
    let snapshot = Owned(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) });
    if snapshot.0 == INVALID_HANDLE_VALUE {
        return Err(last_error("snapshot threads"));
    }
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut resumed = 0;
    // SAFETY: `entry` is a properly sized THREADENTRY32 for the snapshot's iteration.
    let mut more = unsafe { Thread32First(snapshot.0, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: opening and resuming a thread of our own suspended child.
            let thread = Owned(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) });
            if thread.0.is_null() {
                return Err(last_error("open the child's thread"));
            }
            if unsafe { ResumeThread(thread.0) } == u32::MAX {
                return Err(last_error("resume the child's thread"));
            }
            resumed += 1;
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snapshot.0, &mut entry) } != 0;
    }
    if resumed == 0 {
        return Err("the suspended child has no thread to resume".into());
    }
    Ok(())
}

/// Held while inheritable control-pipe handles exist, so no other owned child launched
/// concurrently inherits them.
static SPAWN: Mutex<()> = Mutex::new(());

impl Owned {
    /// The handle as a file, transferring ownership.
    fn into_file(self) -> std::fs::File {
        let h = self.0;
        std::mem::forget(self);
        // SAFETY: the handle is owned and transferred exactly once.
        unsafe { std::fs::File::from_raw_handle(h as _) }
    }
}

/// An anonymous pipe whose `child` end is inheritable and whose host end is not:
/// (read end, write end).
fn pipe(child_reads: bool) -> Result<(Owned, Owned), String> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (mut r, mut w): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: two writable handle out-parameters and valid security attributes.
    if unsafe { CreatePipe(&mut r, &mut w, &sa, 0) } == 0 {
        return Err(last_error("create the control pipe"));
    }
    let (r, w) = (Owned(r), Owned(w));
    let host = if child_reads { &w } else { &r };
    // SAFETY: clearing inheritance on a handle this function owns.
    if unsafe { SetHandleInformation(host.0, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(last_error("keep the control pipe private"));
    }
    Ok((r, w))
}

pub(super) fn spawn(spec: &ProcessSpec) -> Result<Box<dyn ProcessTree>, ProcessError> {
    // SAFETY: an unnamed job with default security; the handle is owned by `Owned`.
    let job = Owned(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) });
    if job.0.is_null() {
        return Err(ProcessError::Ownership(last_error("create the job")));
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    // Kill on close and no breakaway: every descendant stays in the job and dies with it.
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: a correctly sized extended limit structure for this information class.
    let ok = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(ProcessError::Ownership(last_error("configure the job")));
    }
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);
    if spec.clear_env {
        cmd.env_clear();
    }
    cmd.envs(spec.env.iter().map(|(k, v)| (k, v)));
    if let Some(dir) = &spec.cwd {
        cmd.current_dir(dir);
    }
    cmd.stdin(Stdio::null());
    let out = || {
        if spec.capture_output {
            Stdio::piped()
        } else {
            Stdio::null()
        }
    };
    cmd.stdout(out()).stderr(out());
    cmd.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP);
    // A private control channel: two anonymous pipes whose child ends are inherited (and named
    // on the command line); the host ends are not inheritable. The spawn lock keeps the
    // inheritable ends from reaching another owned child launched at the same moment.
    let serial = SPAWN.lock().unwrap_or_else(|p| p.into_inner());
    let mut pipes = None;
    if let Some(c) = &spec.control_pipe {
        let (child_read, host_write) = pipe(true).map_err(ProcessError::Spawn)?;
        let (host_read, child_write) = pipe(false).map_err(ProcessError::Spawn)?;
        let (r, w) = (
            (child_read.0 as usize).to_string(),
            (child_write.0 as usize).to_string(),
        );
        cmd.args(
            c.windows_args
                .iter()
                .map(|a| a.replace("{read}", &r).replace("{write}", &w)),
        );
        pipes = Some((child_read, child_write, host_write, host_read));
    }
    let spawned = cmd.spawn();
    // The child's ends close here: end of file reaches the host when the child exits.
    let control: Option<ControlChannel> = pipes.map(|(cr, cw, hw, hr)| {
        drop((cr, cw));
        (
            Box::new(hw.into_file()) as Box<dyn std::io::Write + Send>,
            Box::new(hr.into_file()) as Box<dyn Read + Send>,
        )
    });
    drop(serial);
    let mut child = spawned.map_err(|e| ProcessError::Spawn(e.to_string()))?;
    let process = child.as_raw_handle() as HANDLE;
    let pid = child.id();
    let fail = |child: &mut Child, e: String| {
        // The child never ran: terminate it rather than leave it suspended and unowned.
        let _ = child.kill();
        let _ = child.wait();
        Err(ProcessError::Ownership(e))
    };
    // SAFETY: assigning our own suspended child to our job.
    if unsafe { AssignProcessToJobObject(job.0, process) } == 0 {
        return fail(&mut child, last_error("assign the child to its job"));
    }
    let birth = creation_time(process).unwrap_or_else(|| "unavailable".into());
    if let Err(e) = resume(pid) {
        // SAFETY: the job only holds this child.
        unsafe { TerminateJobObject(job.0, 1) };
        return fail(&mut child, e);
    }
    let output = match (child.stdout.take(), child.stderr.take()) {
        (Some(o), Some(e)) => Some((
            Box::new(o) as Box<dyn Read + Send>,
            Box::new(e) as Box<dyn Read + Send>,
        )),
        _ => None,
    };
    Ok(Box::new(JobTree {
        job,
        child,
        identity: NativeIdentity {
            pid,
            birth,
            group: None,
        },
        empty: false,
        exit_code: None,
        output,
        control,
    }))
}

struct JobTree {
    job: Owned,
    child: Child,
    identity: NativeIdentity,
    empty: bool,
    exit_code: Option<i32>,
    output: Option<(Box<dyn Read + Send>, Box<dyn Read + Send>)>,
    control: Option<ControlChannel>,
}
// SAFETY: the job handle is a kernel object handle usable from any thread.
unsafe impl Send for JobTree {}

impl JobTree {
    fn active(&self) -> Result<u32, ProcessError> {
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a correctly sized accounting structure for this information class.
        let ok = unsafe {
            QueryInformationJobObject(
                self.job.0,
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(ProcessError::Control(last_error("query the job")));
        }
        Ok(info.ActiveProcesses)
    }
}

impl ProcessTree for JobTree {
    fn identity(&self) -> &NativeIdentity {
        &self.identity
    }
    fn request_stop(&mut self) -> Result<bool, ProcessError> {
        if self.empty {
            return Ok(true);
        }
        // CTRL_BREAK reaches the child's process group only when it shares our console.
        // SAFETY: a console control event for the group this tree's root leads.
        Ok(unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.identity.pid) } != 0)
    }
    fn kill(&mut self) -> Result<(), ProcessError> {
        if self.empty {
            return Ok(());
        }
        // SAFETY: terminating every process of our own job.
        if unsafe { TerminateJobObject(self.job.0, 1) } == 0 {
            return Err(ProcessError::Control(last_error("terminate the job")));
        }
        Ok(())
    }
    fn poll(&mut self) -> Result<TreeStatus, ProcessError> {
        if !self.empty && self.active()? == 0 {
            if let Ok(Some(status)) = self.child.try_wait() {
                self.exit_code = status.code();
            }
            self.empty = true;
        }
        Ok(TreeStatus {
            empty: self.empty,
            exit_code: self.exit_code,
        })
    }
    fn take_output(&mut self) -> Option<(Box<dyn Read + Send>, Box<dyn Read + Send>)> {
        self.output.take()
    }
    fn take_control(&mut self) -> Option<ControlChannel> {
        self.control.take()
    }
}

impl Drop for JobTree {
    fn drop(&mut self) {
        if !self.empty {
            // Closing the job (when `job` drops) would kill the tree too; terminate explicitly
            // so the stop does not depend on handle lifetime.
            let _ = self.kill();
        }
    }
}

fn open_verified(identity: &NativeIdentity, access: u32) -> Result<Owned, Probe> {
    // SAFETY: opening a process by pid for query (and optionally terminate) access.
    let handle =
        Owned(unsafe { OpenProcess(access | PROCESS_QUERY_LIMITED_INFORMATION, 0, identity.pid) });
    if handle.0.is_null() {
        let e = std::io::Error::last_os_error();
        // ERROR_INVALID_PARAMETER: no such process.
        return Err(if e.raw_os_error() == Some(87) {
            Probe::Gone
        } else {
            Probe::Unknown(format!("open the recorded process: {e}"))
        });
    }
    match creation_time(handle.0) {
        Some(b) if b == identity.birth => Ok(handle),
        // Another process now has this pid: the recorded one is gone.
        Some(_) => Err(Probe::Gone),
        None => Err(Probe::Unknown(last_error("read the process start time"))),
    }
}

pub(super) fn probe(identity: &NativeIdentity) -> Probe {
    let handle = match open_verified(identity, 0) {
        Ok(h) => h,
        Err(p) => return p,
    };
    let mut code = 0u32;
    // SAFETY: a valid process handle and a writable exit-code out-parameter.
    if unsafe { GetExitCodeProcess(handle.0, &mut code) } == 0 {
        return Probe::Unknown(last_error("read the exit code"));
    }
    if code == STILL_ACTIVE as u32 {
        Probe::Alive
    } else {
        Probe::Gone
    }
}

pub(super) fn terminate(identity: &NativeIdentity) -> Probe {
    if probe(identity) != Probe::Alive {
        return probe(identity);
    }
    let handle = match open_verified(identity, PROCESS_TERMINATE | PROCESS_SYNCHRONIZE) {
        Ok(h) => h,
        Err(p) => return p,
    };
    // An earlier boot's job closed when that boot ended, killing its members; only a root that
    // somehow survived it remains, so terminate it directly.
    // SAFETY: terminating the verified recorded process and waiting on its handle.
    unsafe {
        TerminateProcess(handle.0, 1);
        WaitForSingleObject(handle.0, 2_000);
    }
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        let p = probe(identity);
        if p != Probe::Alive || Instant::now() >= until {
            return p;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
