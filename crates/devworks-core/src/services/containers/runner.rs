//! Bounded native CLI execution through the shared owned-process supervisor.
use crate::authority::destructive::{FilesystemResolver, ProtectedRoots};
use crate::context::InvocationContext;
use crate::ports::process::ProcessSpec;
use crate::protocol::OperationId;
use crate::services::process::tools::{BoundedOutput, redacted_output};
use crate::services::process::{
    CaptureFault, CapturePolicy, CaptureStream, CaptureWait, LaunchPolicy, LaunchRefusal,
    LaunchRequest, ProcessSupervisor,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CliError {
    Refused(String),
    Unknown(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliResult {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

pub struct ContainerRunner {
    processes: Arc<ProcessSupervisor>,
    filesystem: Arc<dyn FilesystemResolver>,
    protected_roots: ProtectedRoots,
}

impl ContainerRunner {
    pub fn new(
        processes: Arc<ProcessSupervisor>,
        filesystem: Arc<dyn FilesystemResolver>,
        protected_roots: ProtectedRoots,
    ) -> Self {
        Self {
            processes,
            filesystem,
            protected_roots,
        }
    }

    pub fn run(
        &self,
        ctx: &InvocationContext,
        operation: Option<OperationId>,
        spec: ProcessSpec,
        timeout: Duration,
        max_output: usize,
        secrets: &[String],
    ) -> Result<CliResult, CliError> {
        self.run_inner(ctx, operation, spec, timeout, (0, 0, max_output), secrets)
            .map(|(result, _, _)| result)
    }

    /// Drain both CLI streams while retaining one requested byte window from each.
    pub fn run_window(
        &self,
        ctx: &InvocationContext,
        operation: Option<OperationId>,
        spec: ProcessSpec,
        timeout: Duration,
        window: (u64, u64, usize),
        secrets: &[String],
    ) -> Result<(CliResult, u64, u64), CliError> {
        self.run_inner(ctx, operation, spec, timeout, window, secrets)
    }

    fn run_inner(
        &self,
        ctx: &InvocationContext,
        operation: Option<OperationId>,
        spec: ProcessSpec,
        timeout: Duration,
        window: (u64, u64, usize),
        secrets: &[String],
    ) -> Result<(CliResult, u64, u64), CliError> {
        let (stdout_after, stderr_after, max_output) = window;
        if timeout.is_zero() || timeout > Duration::from_secs(3600) || max_output > 1_048_576 {
            return Err(CliError::Refused("container CLI bounds are invalid".into()));
        }
        let deadline = Instant::now() + timeout;
        let policy = LaunchPolicy {
            fs: &*self.filesystem,
            roots: &self.protected_roots,
            protected_branches: &[],
        };
        let process = self
            .processes
            .launch(
                ctx,
                LaunchRequest {
                    spec,
                    operation_id: operation,
                    lease: None,
                    approved_hold: false,
                },
                &policy,
            )
            .map_err(|reason| match reason {
                LaunchRefusal::Storage(error) => {
                    CliError::Unknown(format!("container CLI launch record unavailable: {error}"))
                }
                other => CliError::Refused(format!("container CLI launch refused: {other:?}")),
            })?;
        let Some((stdout, stderr)) = self.processes.take_output(&process) else {
            let _ = self.processes.cancel(
                &process,
                Duration::ZERO,
                deadline.saturating_duration_since(Instant::now()),
            );
            return Err(CliError::Unknown("container CLI output unavailable".into()));
        };
        // Keep the streams separate: successful engine JSON on stdout must remain parseable
        // even when the CLI writes a warning to stderr.
        let stdout_bytes = Arc::new(Mutex::new(BoundedOutput::from_offset(
            max_output,
            stdout_after,
        )));
        let stderr_bytes = Arc::new(Mutex::new(BoundedOutput::from_offset(
            max_output,
            stderr_after,
        )));
        let sink = |output: &Arc<Mutex<BoundedOutput>>| {
            let output = output.clone();
            CaptureStream {
                on_chunk: Box::new(move |chunk| {
                    output
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .append(chunk);
                }),
                on_end: None,
            }
        };
        let exit = self
            .processes
            .run_captured(
                &process,
                (stdout, stderr),
                [sink(&stdout_bytes), sink(&stderr_bytes)],
                CapturePolicy {
                    stop_at: deadline - (timeout / 5).min(Duration::from_secs(1)),
                    term_grace: Duration::ZERO,
                    stop_wait: CaptureWait::Until(deadline),
                    drain_wait: CaptureWait::Until(deadline),
                    drain_stop_wait: Duration::ZERO,
                },
            )
            .map_err(|fault| match fault {
                CaptureFault::StopNotRecorded(error) => {
                    CliError::Unknown(format!("container CLI timeout stop not recorded: {error}"))
                }
                CaptureFault::StopUnverified(_) => {
                    CliError::Unknown("container CLI process stop unverified".into())
                }
                CaptureFault::DrainTimedOut => {
                    CliError::Unknown("container CLI output did not drain".into())
                }
            })?;
        let exit_code = exit.exit_code;
        let stdout_bytes = stdout_bytes.lock().unwrap_or_else(|p| p.into_inner());
        let stderr_bytes = stderr_bytes.lock().unwrap_or_else(|p| p.into_inner());
        let truncated = stdout_bytes.truncated() || stderr_bytes.truncated();
        let stdout_seen = stdout_bytes.seen();
        let stderr_seen = stderr_bytes.seen();
        Ok((
            CliResult {
                exit_code,
                stdout: redacted_output(stdout_bytes.retained(), secrets, truncated),
                stderr: redacted_output(stderr_bytes.retained(), secrets, truncated),
                truncated: truncated
                    || secrets
                        .iter()
                        .any(|value| !value.is_empty() && value.len() < 4),
            },
            stdout_seen,
            stderr_seen,
        ))
    }
}
