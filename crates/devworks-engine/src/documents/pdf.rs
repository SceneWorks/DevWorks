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
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(10);

/// Opens the worker shipped beside the native app. User document data never supplies a path or
/// command. Invocation owns both the private pipe and the native tree until they are collected.
pub struct PdfWorker {
    program: PathBuf,
    process: Arc<dyn ProcessHost>,
    active: AtomicUsize,
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
            active: AtomicUsize::new(0),
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
        let _slot = Slot::take(&self.active)?;
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
        let reply = supervise(&mut *child, channel, source.bytes, operation, &control)?;
        check(&control)?;
        reply.validate(source.version.sha256.as_str(), operation)?;
        match reply.result {
            PdfResult::Failed { error } => Err(error),
            value => Ok(value),
        }
    }
}

struct Slot<'a>(&'a AtomicUsize);

impl<'a> Slot<'a> {
    fn take(active: &'a AtomicUsize) -> Result<Self, PdfFailure> {
        active
            .fetch_update(Ordering::Acquire, Ordering::Relaxed, |count| {
                (count < 2).then_some(count + 1)
            })
            .map_err(|_| PdfFailure::Busy)?;
        Ok(Self(active))
    }
}

impl Drop for Slot<'_> {
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
    child: &mut dyn ProcessTree,
    channel: ControlChannel,
    bytes: &[u8],
    operation: PdfOperation,
    control: &ExtractionControl<'_>,
) -> Result<PdfReply, PdfFailure> {
    thread::scope(|scope| {
        let (send, recv) = mpsc::sync_channel(1);
        scope.spawn(move || {
            let _ = send.send(exchange(channel, bytes, operation));
        });
        let result = wait_for_reply(child, &recv, control);
        if result.is_err() {
            let _ = child.kill();
            let until = Instant::now() + Duration::from_secs(2);
            while Instant::now() < until && child.poll().is_ok_and(|state| !state.empty) {
                thread::sleep(POLL);
            }
        }
        result
    })
}

fn wait_for_reply(
    child: &mut dyn ProcessTree,
    recv: &mpsc::Receiver<Result<PdfReply, PdfFailure>>,
    control: &ExtractionControl<'_>,
) -> Result<PdfReply, PdfFailure> {
    let mut reply = None;
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
        if reply.is_none() {
            match recv.try_recv() {
                Ok(result) => reply = Some(result?),
                Err(mpsc::TryRecvError::Disconnected) => return Err(PdfFailure::InvalidResponse),
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
