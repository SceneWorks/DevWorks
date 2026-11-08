//! Bounded native CLI execution through the shared owned-process supervisor.
use crate::authority::destructive::{FilesystemResolver, ProtectedRoots};
use crate::context::InvocationContext;
use crate::ports::process::ProcessSpec;
use crate::protocol::OperationId;
use crate::services::process::tools::{BoundedOutput, redacted_output};
use crate::services::process::{
    LaunchPolicy, LaunchRefusal, LaunchRequest, ProcessSupervisor, StopOutcome,
};
use std::sync::{Arc, Mutex, mpsc};
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
        if timeout.is_zero() || timeout > Duration::from_secs(3600) || max_output > 1_048_576 {
            return Err(CliError::Refused("container CLI bounds are invalid".into()));
        }
        let policy = LaunchPolicy {
            fs: &*self.filesystem,
            roots: &self.protected_roots,
            protected_branches: &["main".into(), "master".into()],
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
            let _ = self
                .processes
                .cancel(&process, Duration::ZERO, Duration::from_secs(5));
            return Err(CliError::Unknown("container CLI output unavailable".into()));
        };
        // Keep the streams separate: successful engine JSON on stdout must remain parseable
        // even when the CLI writes a warning to stderr.
        let stdout_bytes = Arc::new(Mutex::new(BoundedOutput::new(max_output)));
        let stderr_bytes = Arc::new(Mutex::new(BoundedOutput::new(max_output)));
        let (drained_tx, drained_rx) = mpsc::channel();
        for (mut pipe, output) in [
            (stdout, stdout_bytes.clone()),
            (stderr, stderr_bytes.clone()),
        ] {
            let drained_tx = drained_tx.clone();
            std::thread::spawn(move || {
                use std::io::Read;
                let mut bytes = [0u8; 8192];
                while let Ok(n) = pipe.read(&mut bytes) {
                    if n == 0 {
                        break;
                    }
                    output
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .append(&bytes[..n]);
                }
                let _ = drained_tx.send(());
            });
        }
        drop(drained_tx);
        let deadline = Instant::now() + timeout;
        let outcome = loop {
            match self.processes.poll(&process) {
                Ok(Some(outcome)) => break outcome,
                Ok(None) => {}
                Err(error) => {
                    let _ = self
                        .processes
                        .cancel(&process, Duration::ZERO, Duration::from_secs(5));
                    return Err(CliError::Unknown(format!(
                        "container CLI poll failed: {error}"
                    )));
                }
            }
            if Instant::now() >= deadline {
                break self
                    .processes
                    .cancel(&process, Duration::from_secs(1), Duration::from_secs(5))
                    .map_err(|failure| {
                        CliError::Unknown(format!(
                            "container CLI timeout stop not recorded: {}",
                            failure.error
                        ))
                    })?;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let StopOutcome::Reaped { exit_code, .. } = outcome else {
            return Err(CliError::Unknown(
                "container CLI process stop unverified".into(),
            ));
        };
        for _ in 0..2 {
            if drained_rx.recv_timeout(Duration::from_secs(5)).is_err() {
                let _ = self
                    .processes
                    .cancel(&process, Duration::ZERO, Duration::from_secs(5));
                return Err(CliError::Unknown(
                    "container CLI output did not drain".into(),
                ));
            }
        }
        let stdout_bytes = stdout_bytes.lock().unwrap_or_else(|p| p.into_inner());
        let stderr_bytes = stderr_bytes.lock().unwrap_or_else(|p| p.into_inner());
        let truncated = stdout_bytes.truncated() || stderr_bytes.truncated();
        Ok(CliResult {
            exit_code,
            stdout: redacted_output(stdout_bytes.retained(), secrets, truncated),
            stderr: redacted_output(stderr_bytes.retained(), secrets, truncated),
            truncated: truncated
                || secrets
                    .iter()
                    .any(|value| !value.is_empty() && value.len() < 4),
        })
    }
}
