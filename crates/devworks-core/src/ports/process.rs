//! Owned process trees and the OS power request (the process owner; POWER-01/02).
//!
//! [`ProcessHost`] is the native launcher: `devworks-adapters::process` implements it with owned
//! process groups on macOS/Linux and a kill-on-close, non-breakaway Job Object on Windows.
//! Callers never spawn through it directly: [`crate::services::process::ProcessSupervisor`]
//! runs the shell/process safety preflight, the dispatch-time resource check and the durable
//! launch record first, and owns bounded cancellation and crash recovery.
//!
//! A native identity is a pid **plus** a birth token (the process start time), so a reused pid
//! is never mistaken for the recorded process. A process group or job is not an inescapable
//! sandbox: a descendant that leaves it is not tracked, and the host reports what it could
//! verify instead of claiming a stop from the root's exit.
use std::io::{Read, Write};
use std::path::PathBuf;

/// What to launch. The program is resolved by the OS from `program` (a path or a name on
/// `PATH`); arguments are passed as separate words, never through a shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Variables set for the child (after clearing, when `clear_env`).
    pub env: Vec<(String, String)>,
    pub clear_env: bool,
    /// Capture stdout and stderr as pipes ([`ProcessTree::take_output`]); otherwise discard.
    pub capture_output: bool,
    /// Give the child a private control channel ([`ProcessTree::take_control`]) that no other
    /// process can reach: anonymous pipes, never a name or a port.
    pub control_pipe: Option<ControlPipe>,
}

/// A private bidirectional control channel of a child. On Unix the child reads what the host
/// writes from file descriptor 3 and writes what the host reads to file descriptor 4. On
/// Windows the child inherits two anonymous pipe handles, named on its command line by
/// [`Self::windows_args`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlPipe {
    /// Windows only: arguments appended at launch, with `{read}` replaced by the decimal value
    /// of the handle the child reads and `{write}` by the handle it writes.
    pub windows_args: Vec<String>,
}

/// The host's ends of a control channel: what it writes to the child, what it reads from it.
pub type ControlChannel = (Box<dyn Write + Send>, Box<dyn Read + Send>);
impl ProcessSpec {
    pub fn new(argv: &[String]) -> Option<Self> {
        let (program, args) = argv.split_first()?;
        Some(Self {
            program: program.clone(),
            args: args.to_vec(),
            cwd: None,
            env: Vec::new(),
            clear_env: false,
            capture_output: false,
            control_pipe: None,
        })
    }
    /// The program followed by its arguments.
    pub fn argv(&self) -> Vec<String> {
        std::iter::once(self.program.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }
}

/// Native identity of a launched tree's root, as recorded for recovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeIdentity {
    pub pid: u32,
    /// Platform start-time token of `pid`; a different token means a different process.
    pub birth: String,
    /// The Unix process group (equal to the root's pid). Windows uses an unnamed job that dies
    /// with this process, so it records none.
    pub group: Option<i64>,
}

/// One observation of an owned tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeStatus {
    /// No live member of the tree remains (the root's own exit is not enough).
    pub empty: bool,
    /// The root's exit code, once the tree is empty and the root was collected.
    pub exit_code: Option<i32>,
}

/// What the host can say about a recorded identity (recovery of an earlier boot).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// Neither the recorded root nor any member of its group is alive.
    Gone,
    /// The recorded root (same pid and birth token) is alive.
    Alive,
    /// Neither can be established (permission, a lingering group without its root, an
    /// unreadable process table); the reason is diagnostic text.
    Unknown(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessError {
    /// The program could not be started; nothing runs.
    Spawn(String),
    /// The tree could not be placed under ownership; the host killed what it started.
    Ownership(String),
    /// A signal, job or wait call failed.
    Control(String),
}
impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "spawn failed: {e}"),
            Self::Ownership(e) => write!(f, "could not own the process tree: {e}"),
            Self::Control(e) => write!(f, "process control failed: {e}"),
        }
    }
}

/// A launched tree the host owns. Dropping it forcibly kills whatever of the tree remains
/// (best effort), so an owner that disappears never leaves the tree running silently.
pub trait ProcessTree: Send {
    fn identity(&self) -> &NativeIdentity;
    /// Requests a cooperative stop of the whole tree (SIGTERM to the group; CTRL_BREAK on
    /// Windows). `Ok(false)` means the platform offers none for this tree.
    fn request_stop(&mut self) -> Result<bool, ProcessError>;
    /// Forcibly terminates every member of the tree (SIGKILL to the group; TerminateJobObject).
    fn kill(&mut self) -> Result<(), ProcessError>;
    /// Observes the tree without blocking. Once it reports `empty`, the host never signals
    /// the recorded group again (its identifier may be reused).
    fn poll(&mut self) -> Result<TreeStatus, ProcessError>;
    /// The captured stdout and stderr pipes, once.
    fn take_output(&mut self) -> Option<(Box<dyn Read + Send>, Box<dyn Read + Send>)>;
    /// The host's ends of the control channel of a tree launched with a control pipe, once.
    fn take_control(&mut self) -> Option<ControlChannel> {
        None
    }
    /// Whether the tree's root has exited, whatever its descendants do (a program whose main
    /// process died while helpers linger has ended). Never collects anything. The default is
    /// the tree's emptiness.
    fn root_exited(&mut self) -> Result<bool, ProcessError> {
        self.poll().map(|s| s.empty)
    }
}

/// The native launcher.
pub trait ProcessHost: Send + Sync {
    /// `unix` or `windows`.
    fn platform(&self) -> &'static str;
    /// The environment a launched child inherits (unless its spec clears it): the safety
    /// preflight resolves `~` and variable references against it plus the spec's own variables.
    fn environment(&self) -> Vec<(String, String)>;
    /// Starts `spec` as an owned tree. On Windows the child is assigned to its job before it
    /// runs; on Unix it leads a new process group.
    fn spawn(&self, spec: &ProcessSpec) -> Result<Box<dyn ProcessTree>, ProcessError>;
    /// Inspects a recorded identity without changing anything.
    fn probe(&self, identity: &NativeIdentity) -> Probe;
    /// Forcibly terminates the tree of a recorded identity when the root is verified to be
    /// the recorded process, and reports what remains within a short bound.
    fn terminate(&self, identity: &NativeIdentity) -> Probe;
}

// ---------------------------------------------------------------------------------------------
// Power

/// Whether the OS accepted an idle-sleep prevention request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PowerRequest {
    /// The request is held: idle system sleep is prevented, display sleep is allowed.
    Effective { mechanism: &'static str },
    /// No request could be made on this system; the reason is shown with the setting.
    Unavailable(String),
}

/// A held OS power request. Dropping it releases the request.
pub trait PowerAssertion: Send {
    /// Re-checks the request (for example that its helper is still running).
    fn status(&mut self) -> PowerRequest;
}

/// The OS power-management facility (macOS `caffeinate -i`, Windows SetThreadExecutionState,
/// Linux logind idle inhibitor). It never prevents explicit sleep, lid close or shutdown.
pub trait PowerPort: Send + Sync {
    /// Requests prevention of idle system sleep while allowing display sleep.
    fn prevent_idle_sleep(&self) -> Result<Box<dyn PowerAssertion>, String>;
}
