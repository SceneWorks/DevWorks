//! Helper processes of indexing (Git), launched only through the DW-006 process owner
//! ([`ProcessSupervisor`]): every launch passes its safety preflight and durable launch record,
//! output is read with fixed bounds, and a run's cancellation or deadline kills the whole tree
//! (CODE-08). A helper never outlives the call that started it.
use crate::authority::destructive::{FilesystemResolver, ProtectedRoots};
use crate::authority::redact::Redactor;
use crate::context::InvocationContext;
use crate::ports::process::ProcessSpec;
use crate::ports::storage::{ApplicationStore, StorageError};
use crate::protocol::OperationId;
use crate::services::process::{
    LaunchPolicy, LaunchRefusal, LaunchRequest, ProcessSupervisor, StopOutcome,
};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Bounds of one helper call.
#[derive(Clone, Copy, Debug)]
pub struct HelperLimits {
    pub deadline: Duration,
    /// Stdout beyond this kills the tree (`OutputTooLarge`): output is never truncated silently.
    pub max_stdout: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HelperFault {
    /// The run was cancelled; the tree was killed.
    Cancelled,
    /// The deadline passed; the tree was killed.
    TimedOut,
    /// More output than the bound; the tree was killed.
    OutputTooLarge,
    /// The process owner refused the launch (preflight, archived project); nothing ran.
    Refused(String),
    /// The program could not be started.
    Spawn(String),
    /// It exited unsuccessfully; `stderr` is bounded and redacted.
    Failed {
        code: Option<i32>,
        stderr: String,
    },
    /// The stop of a killed tree could not be verified.
    Uncertain(String),
    Storage(String),
}
impl std::fmt::Display for HelperFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::TimedOut => f.write_str("timed out"),
            Self::OutputTooLarge => f.write_str("output exceeded its bound"),
            Self::Refused(r) => write!(f, "refused: {r}"),
            Self::Spawn(e) => write!(f, "could not start: {e}"),
            Self::Failed { code, stderr } => match code {
                Some(c) => write!(f, "exited with {c}: {stderr}"),
                None => write!(f, "terminated: {stderr}"),
            },
            Self::Uncertain(e) => write!(f, "stop not verified: {e}"),
            Self::Storage(e) => write!(f, "storage: {e}"),
        }
    }
}

/// Runs bounded helper processes.
pub trait HelperRunner: Send + Sync {
    fn filesystem(&self) -> Option<&dyn FilesystemResolver> {
        None
    }

    /// Host environment for helpers requiring a filtered inheritance boundary.
    fn environment(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Runs a journaled mutation. Implementations must retain the operation identity through
    /// launch and must refuse rather than silently downgrade to an unrecorded process.
    fn run_recorded(
        &self,
        _ctx: &InvocationContext,
        _spec: ProcessSpec,
        _limits: HelperLimits,
        _stop: &(dyn Fn() -> bool + Sync),
        _operation: &OperationId,
        _approved_hold: bool,
    ) -> Result<Vec<u8>, HelperFault> {
        Err(HelperFault::Refused(
            "journaled helpers are unsupported".into(),
        ))
    }

    /// A read-only Git no-index binary diff uses exit 1 to report a difference. Unsupported
    /// adapters refuse rather than discard evidence or silently treat another program as Git.
    fn run_difference(
        &self,
        _ctx: &InvocationContext,
        _spec: ProcessSpec,
        _limits: HelperLimits,
        _stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, HelperFault> {
        Err(HelperFault::Refused(
            "bounded Git difference reads are unsupported".into(),
        ))
    }
    fn run_recorded_difference(
        &self,
        _ctx: &InvocationContext,
        _spec: ProcessSpec,
        _limits: HelperLimits,
        _stop: &(dyn Fn() -> bool + Sync),
        _operation: &OperationId,
    ) -> Result<Vec<u8>, HelperFault> {
        Err(HelperFault::Refused(
            "journaled Git difference reads are unsupported".into(),
        ))
    }

    /// Runs `spec` (its output is captured) for `ctx`, returning stdout once it exits 0.
    fn run(
        &self,
        ctx: &InvocationContext,
        spec: ProcessSpec,
        limits: HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, HelperFault>;
}

/// [`HelperRunner`] over the process owner.
pub struct SupervisedHelpers {
    pub supervisor: Arc<ProcessSupervisor>,
    pub store: Arc<dyn ApplicationStore>,
    pub fs: Arc<dyn FilesystemResolver>,
}

/// How long a killed tree may take to be verified gone.
const STOP_BOUND: Duration = Duration::from_secs(5);
const STDERR_BOUND: usize = 16 * 1024;

fn reader(
    mut r: Box<dyn Read + Send>,
    cap: usize,
    overflow: Arc<AtomicBool>,
) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut buf = [0u8; 64 * 1024];
        let mut over = false;
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if over || out.len() + n > cap {
                        over = true;
                        overflow.store(true, Ordering::SeqCst);
                        // Keep draining (so the child is not blocked on a full pipe before the
                        // kill lands) without keeping more.
                        continue;
                    }
                    out.extend_from_slice(&buf[..n]);
                }
            }
        }
        (out, over)
    })
}

impl HelperRunner for SupervisedHelpers {
    fn filesystem(&self) -> Option<&dyn FilesystemResolver> {
        Some(&*self.fs)
    }

    fn environment(&self) -> Vec<(String, String)> {
        self.supervisor.environment()
    }

    fn run(
        &self,
        ctx: &InvocationContext,
        spec: ProcessSpec,
        limits: HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, HelperFault> {
        self.run_owned(ctx, spec, limits, stop, None, false, false)
    }

    fn run_recorded(
        &self,
        ctx: &InvocationContext,
        spec: ProcessSpec,
        limits: HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
        operation: &OperationId,
        approved_hold: bool,
    ) -> Result<Vec<u8>, HelperFault> {
        self.run_owned(
            ctx,
            spec,
            limits,
            stop,
            Some(operation.clone()),
            approved_hold,
            false,
        )
    }
    fn run_difference(
        &self,
        ctx: &InvocationContext,
        spec: ProcessSpec,
        limits: HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, HelperFault> {
        difference_command(&spec)?;
        self.run_owned(ctx, spec, limits, stop, None, false, true)
    }
    fn run_recorded_difference(
        &self,
        ctx: &InvocationContext,
        spec: ProcessSpec,
        limits: HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
        operation: &OperationId,
    ) -> Result<Vec<u8>, HelperFault> {
        difference_command(&spec)?;
        self.run_owned(
            ctx,
            spec,
            limits,
            stop,
            Some(operation.clone()),
            false,
            true,
        )
    }
}
fn difference_command(spec: &ProcessSpec) -> Result<(), HelperFault> {
    if spec.args.first().is_none_or(|s| s != "diff")
        || !spec.args.iter().any(|s| s == "--no-index")
        || !spec.args.iter().any(|s| s == "--binary")
        || !spec.args.iter().any(|s| s == "--no-ext-diff")
        || !spec.args.iter().any(|s| s == "--no-textconv")
    {
        return Err(HelperFault::Refused(
            "difference reads require a hardened Git no-index binary diff".into(),
        ));
    }
    Ok(())
}
impl SupervisedHelpers {
    fn run_owned(
        &self,
        ctx: &InvocationContext,
        mut spec: ProcessSpec,
        limits: HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
        operation: Option<OperationId>,
        approved_hold: bool,
        difference: bool,
    ) -> Result<Vec<u8>, HelperFault> {
        if stop() {
            return Err(HelperFault::Cancelled);
        }
        spec.capture_output = true;
        let roots = ProtectedRoots::read(&*self.store, Vec::new(), None)
            .map_err(|e: StorageError| HelperFault::Storage(e.to_string()))?;
        let policy = LaunchPolicy {
            fs: &*self.fs,
            roots: &roots,
            protected_branches: &[],
        };
        let owned = self
            .supervisor
            .launch(
                ctx,
                LaunchRequest {
                    spec,
                    operation_id: operation,
                    lease: None,
                    approved_hold,
                },
                &policy,
            )
            .map_err(|r| match r {
                LaunchRefusal::Spawn(e) => HelperFault::Spawn(e.to_string()),
                LaunchRefusal::Storage(e) => HelperFault::Storage(e.to_string()),
                LaunchRefusal::ProjectClosed => {
                    HelperFault::Refused("project closed to work".into())
                }
                other => HelperFault::Refused(format!("{other:?}").chars().take(512).collect()),
            })?;
        let Some((out, err)) = self.supervisor.take_output(&owned) else {
            let _ = self.supervisor.cancel(&owned, Duration::ZERO, STOP_BOUND);
            return Err(HelperFault::Spawn("no output pipes".into()));
        };
        let overflow = Arc::new(AtomicBool::new(false));
        let out = reader(out, limits.max_stdout, overflow.clone());
        let err = reader(err, STDERR_BOUND, Arc::new(AtomicBool::new(false)));
        let started = Instant::now();
        // One kill path for every early end: cancellation, the deadline and runaway output.
        let kill = |why: HelperFault| -> HelperFault {
            match self.supervisor.cancel(&owned, Duration::ZERO, STOP_BOUND) {
                Ok(StopOutcome::Uncertain(e)) => HelperFault::Uncertain(e),
                Err(f) => match f.outcome {
                    StopOutcome::Uncertain(e) => HelperFault::Uncertain(e),
                    _ => why,
                },
                _ => why,
            }
        };
        let exit = loop {
            match self.supervisor.poll(&owned) {
                Ok(Some(StopOutcome::Reaped { exit_code, .. })) => break exit_code,
                Ok(Some(StopOutcome::Uncertain(e))) => return Err(HelperFault::Uncertain(e)),
                Ok(None) => {}
                Err(e) => return Err(kill(HelperFault::Storage(e.to_string()))),
            }
            if stop() {
                return Err(kill(HelperFault::Cancelled));
            }
            if started.elapsed() >= limits.deadline {
                return Err(kill(HelperFault::TimedOut));
            }
            if overflow.load(Ordering::SeqCst) {
                return Err(kill(HelperFault::OutputTooLarge));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let (stdout, over) = out.join().unwrap_or_default();
        let (stderr, _) = err.join().unwrap_or_default();
        if over {
            return Err(HelperFault::OutputTooLarge);
        }
        if exit != Some(0) && !(difference && exit == Some(1)) {
            let text = String::from_utf8_lossy(&stderr);
            let text: String = Redactor::new()
                .redact_text(text.trim())
                .chars()
                .take(2048)
                .collect();
            return Err(HelperFault::Failed {
                code: exit,
                stderr: text,
            });
        }
        Ok(stdout)
    }
}

/// Every helper belonging to an already committed Git effect retains that operation, including
/// readback and remote observations whose Git transport may invoke configured credentials.
pub struct RecordedHelpers<'a> {
    pub inner: &'a dyn HelperRunner,
    pub operation: &'a OperationId,
}
impl HelperRunner for RecordedHelpers<'_> {
    fn filesystem(&self) -> Option<&dyn FilesystemResolver> {
        self.inner.filesystem()
    }
    fn environment(&self) -> Vec<(String, String)> {
        self.inner.environment()
    }
    fn run(
        &self,
        ctx: &InvocationContext,
        spec: ProcessSpec,
        limits: HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, HelperFault> {
        self.inner
            .run_recorded(ctx, spec, limits, stop, self.operation, false)
    }
    fn run_difference(
        &self,
        ctx: &InvocationContext,
        spec: ProcessSpec,
        limits: HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<u8>, HelperFault> {
        self.inner
            .run_recorded_difference(ctx, spec, limits, stop, self.operation)
    }
    fn run_recorded_difference(
        &self,
        ctx: &InvocationContext,
        spec: ProcessSpec,
        limits: HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
        operation: &OperationId,
    ) -> Result<Vec<u8>, HelperFault> {
        if operation != self.operation {
            return Err(HelperFault::Refused("Git helper operation changed".into()));
        }
        self.inner
            .run_recorded_difference(ctx, spec, limits, stop, operation)
    }
    fn run_recorded(
        &self,
        ctx: &InvocationContext,
        spec: ProcessSpec,
        limits: HelperLimits,
        stop: &(dyn Fn() -> bool + Sync),
        operation: &OperationId,
        approved_hold: bool,
    ) -> Result<Vec<u8>, HelperFault> {
        if operation != self.operation {
            return Err(HelperFault::Refused("Git helper operation changed".into()));
        }
        self.inner
            .run_recorded(ctx, spec, limits, stop, operation, approved_hold)
    }
}
