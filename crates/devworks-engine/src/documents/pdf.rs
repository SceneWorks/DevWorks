use devworks_core::{
    ports::process::{ControlChannel, ControlPipe, ProcessHost, ProcessSpec, ProcessTree},
    services::documents::{
        extraction::{ExtractionControl, ExtractionError, ExtractionInput},
        pdf::{
            INPUT_LIMIT, PdfFailure, PdfOperation, PdfReply, PdfResult, READY_MAGIC, REQUEST_MAGIC,
            WORKER_ARGUMENT, WORKER_MEMORY, WORKER_NAME, WORKER_SECONDS,
        },
    },
};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(10);
/// How long a transport failure waits for the worker's exit before it is reported as is.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Opens the worker shipped beside the native app. User document data never supplies a path or
/// command. Invocation owns both the private pipe and the native tree until they are collected.
pub struct PdfWorker {
    program: PathBuf,
    process: Arc<dyn ProcessHost>,
    active: Arc<AtomicUsize>,
    worker_unconfirmed: AtomicBool,
}

impl PdfWorker {
    pub fn beside_desktop(
        desktop: &Path,
        process: Arc<dyn ProcessHost>,
    ) -> Result<Self, PdfFailure> {
        let dir = desktop
            .parent()
            .filter(|dir| dir.is_absolute())
            .ok_or(PdfFailure::Unavailable)?;
        let program = dir.join(format!("{WORKER_NAME}{}", std::env::consts::EXE_SUFFIX));
        Ok(Self {
            program,
            process,
            active: Arc::new(AtomicUsize::new(0)),
            worker_unconfirmed: AtomicBool::new(false),
        })
    }

    pub fn run(
        &self,
        source: ExtractionInput<'_>,
        operation: PdfOperation,
        control: &ExtractionControl<'_>,
    ) -> Result<PdfResult, PdfFailure> {
        let cap = Instant::now() + Duration::from_secs(WORKER_SECONDS);
        let control = ExtractionControl {
            cancelled: control.cancelled,
            deadline: control.deadline.min(cap),
        };
        check(&control)?;
        if self.worker_unconfirmed.load(Ordering::Acquire) {
            return Err(PdfFailure::CleanupUnconfirmed);
        }
        if PdfOperation::from_number(operation.number())? != operation {
            return Err(PdfFailure::InvalidRequest);
        }
        source.verify(INPUT_LIMIT).map_err(|error| match error {
            ExtractionError::SourceTooLarge => PdfFailure::InputLimit,
            _ => PdfFailure::SourceMismatch,
        })?;
        let media = source.version.media_type.split(';').next().unwrap().trim();
        if !media.eq_ignore_ascii_case("application/pdf") {
            return Err(PdfFailure::InvalidRequest);
        }
        if !std::fs::symlink_metadata(&self.program).is_ok_and(|metadata| metadata.is_file()) {
            return Err(PdfFailure::Unavailable);
        }
        let slot = Arc::new(Slot::take(self.active.clone())?);
        check(&control)?;
        let program = self
            .program
            .to_str()
            .ok_or(PdfFailure::Unavailable)?
            .to_owned();
        let mut spec = ProcessSpec::new(&[program, WORKER_ARGUMENT.to_owned()]).expect("program");
        spec.cwd = self.program.parent().map(Path::to_path_buf);
        spec.clear_env = true;
        spec.no_child_processes = true;
        if cfg!(any(target_os = "linux", windows)) {
            spec.memory_limit_bytes = Some(WORKER_MEMORY);
        }
        spec.control_pipe = Some(ControlPipe {
            windows_args: ["{read}", "{write}"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        });
        let mut child = self
            .process
            .spawn(&spec)
            .map_err(|_| PdfFailure::Unavailable)?;
        let channel = child.take_control().ok_or(PdfFailure::Unavailable)?;
        let reply = supervise(
            child,
            channel,
            source.bytes.to_vec(),
            operation,
            &control,
            slot,
        )
        .inspect_err(|error| {
            if *error == PdfFailure::CleanupUnconfirmed {
                self.worker_unconfirmed.store(true, Ordering::Release);
            }
        })?;
        check(&control)?;
        reply.validate(source.version.sha256.as_str(), operation)?;
        match reply.result {
            PdfResult::Failed { error } => Err(error),
            value => Ok(value),
        }
    }
}

struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(active: Arc<AtomicUsize>) -> Result<Self, PdfFailure> {
        active
            .fetch_update(Ordering::Acquire, Ordering::Relaxed, |count| {
                (count < 2).then_some(count + 1)
            })
            .map_err(|_| PdfFailure::Busy)?;
        Ok(Self(active))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

fn check(control: &ExtractionControl<'_>) -> Result<(), PdfFailure> {
    control.check().map_err(|error| match error {
        ExtractionError::Cancelled => PdfFailure::Cancelled,
        _ => PdfFailure::Deadline,
    })
}

fn supervise(
    mut child: Box<dyn ProcessTree>,
    channel: ControlChannel,
    bytes: Vec<u8>,
    operation: PdfOperation,
    control: &ExtractionControl<'_>,
    slot: Arc<Slot>,
) -> Result<PdfReply, PdfFailure> {
    let (send, recv) = mpsc::sync_channel(1);
    let transport_slot = slot.clone();
    // A broken OS pipe can outlive the tree. The transport retains a launch permit until its
    // descriptors close, but the caller's deadline never depends on joining a blocking read.
    let started = thread::Builder::new()
        .name("devworks-pdf-pipe".into())
        .spawn(move || {
            let _permit = transport_slot;
            let _ = send.send(exchange(channel, &bytes, operation));
        });
    let result = match started {
        Ok(_) => wait_for_reply(&mut *child, &recv, control),
        Err(_) => Err(PdfFailure::Unavailable),
    };
    if result.is_err() && !stop_and_reap(&mut *child) {
        return Err(PdfFailure::CleanupUnconfirmed);
    }
    result
}

fn stop_and_reap(child: &mut dyn ProcessTree) -> bool {
    let _ = child.kill();
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        if child.poll().is_ok_and(|state| state.empty) {
            return true;
        }
        thread::sleep(POLL);
    }
    false
}

fn wait_for_reply(
    child: &mut dyn ProcessTree,
    recv: &mpsc::Receiver<Result<PdfReply, PdfFailure>>,
    control: &ExtractionControl<'_>,
) -> Result<PdfReply, PdfFailure> {
    let mut reply = None;
    // A worker that dies mid-reply closes its pipe before its exit is observed, so the
    // transport reports a short read first. Hold that error until the exit shows whether the
    // worker crashed, for at most EXIT_GRACE.
    let mut failed: Option<(PdfFailure, Instant)> = None;
    loop {
        check(control)?;
        let tree = child.poll().map_err(|_| PdfFailure::Crashed)?;
        if tree.empty && tree.exit_code != Some(0) {
            return Err(PdfFailure::Crashed);
        }
        if !tree.empty
            && child
                .resident_memory_bytes()
                .is_some_and(|bytes| bytes > WORKER_MEMORY as u64)
        {
            return Err(PdfFailure::MemoryLimit);
        }
        if let Some((error, at)) = &failed
            && (tree.empty || at.elapsed() >= EXIT_GRACE)
        {
            return Err(*error);
        }
        if reply.is_none() && failed.is_none() {
            match recv.try_recv() {
                Ok(Ok(result)) => reply = Some(result),
                Ok(Err(error)) => failed = Some((error, Instant::now())),
                Err(mpsc::TryRecvError::Disconnected) => {
                    failed = Some((PdfFailure::InvalidResponse, Instant::now()))
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if tree.empty
            && let Some(reply) = reply.take()
        {
            return Ok(reply);
        }
        thread::sleep(POLL.min(control.deadline.saturating_duration_since(Instant::now())));
    }
}

fn exchange(
    (mut sink, mut source): ControlChannel,
    bytes: &[u8],
    operation: PdfOperation,
) -> Result<PdfReply, PdfFailure> {
    let mut ready = [0; 4];
    source
        .read_exact(&mut ready)
        .map_err(|_| PdfFailure::Crashed)?;
    if ready != READY_MAGIC {
        return Err(PdfFailure::InvalidResponse);
    }
    let length = u32::try_from(bytes.len()).map_err(|_| PdfFailure::InputLimit)?;
    sink.write_all(&REQUEST_MAGIC)
        .and_then(|_| sink.write_all(&operation.number().to_le_bytes()))
        .and_then(|_| sink.write_all(&length.to_le_bytes()))
        .and_then(|_| sink.write_all(bytes))
        .map_err(|_| PdfFailure::Crashed)?;
    drop(sink);

    let limit = operation.wire_limit();
    let mut output = Vec::new();
    source
        .take(limit as u64 + 1)
        .read_to_end(&mut output)
        .map_err(|_| PdfFailure::Crashed)?;
    if output.len() > limit {
        return Err(PdfFailure::OutputLimit);
    }
    serde_json::from_slice(&output).map_err(|_| PdfFailure::InvalidResponse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use devworks_core::ports::process::{NativeIdentity, ProcessError, TreeStatus};

    /// A tree that reports `running` polls as live, then exits with `exit_code`.
    struct Exiting {
        identity: NativeIdentity,
        running: usize,
        exit_code: Option<i32>,
    }

    impl ProcessTree for Exiting {
        fn identity(&self) -> &NativeIdentity {
            &self.identity
        }
        fn request_stop(&mut self) -> Result<bool, ProcessError> {
            Ok(true)
        }
        fn kill(&mut self) -> Result<(), ProcessError> {
            Ok(())
        }
        fn poll(&mut self) -> Result<TreeStatus, ProcessError> {
            let empty = self.running == 0;
            self.running = self.running.saturating_sub(1);
            Ok(TreeStatus {
                empty,
                exit_code: if empty { self.exit_code } else { None },
                signal: None,
            })
        }
        fn take_output(&mut self) -> Option<(Box<dyn Read + Send>, Box<dyn Read + Send>)> {
            None
        }
    }

    fn outcome(transport: PdfFailure, running: usize, exit_code: Option<i32>) -> PdfFailure {
        let mut tree = Exiting {
            identity: NativeIdentity {
                pid: 1,
                birth: "test".into(),
                group: None,
            },
            running,
            exit_code,
        };
        let (send, recv) = mpsc::sync_channel(1);
        // The transport fails before the first poll sees the tree end.
        send.send(Err(transport)).unwrap();
        let control = ExtractionControl {
            deadline: Instant::now() + Duration::from_secs(10),
            cancelled: &|| false,
        };
        wait_for_reply(&mut tree, &recv, &control).unwrap_err()
    }

    #[test]
    fn a_short_read_before_an_abnormal_exit_is_a_crash() {
        assert_eq!(
            outcome(PdfFailure::InvalidResponse, 3, Some(101)),
            PdfFailure::Crashed
        );
        assert_eq!(
            outcome(PdfFailure::InvalidResponse, 3, None),
            PdfFailure::Crashed
        );
    }

    #[test]
    fn a_malformed_reply_from_a_normal_exit_stays_invalid() {
        assert_eq!(
            outcome(PdfFailure::InvalidResponse, 3, Some(0)),
            PdfFailure::InvalidResponse
        );
    }

    #[test]
    fn a_transport_failure_from_a_live_worker_is_reported_after_the_grace() {
        let started = Instant::now();
        assert_eq!(
            outcome(PdfFailure::InvalidResponse, usize::MAX, Some(0)),
            PdfFailure::InvalidResponse
        );
        assert!(started.elapsed() >= EXIT_GRACE);
    }
}
