//! Managed semantic helper. Stdout carries only SDK frames.
pub mod process;
pub use devworks_extension_protocol::package::IndexFormat;
pub use process::Failure;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use devworks_core::fingerprint::sha256_hex;
use devworks_extension_protocol::{
    frame::{FrameError, MAX_FRAME_BYTES, read_frame, write_frame},
    package::{
        ARTIFACT_CHUNK_BYTES, BrokerCall, BrokerInvoke, ContributionEvent, HostRequest,
        InvocationEvent, InvocationOutcome, Invoke, PackageNotification, PackageRequest,
        RpcFailure, RpcFailureId, RpcResult, RpcSuccess, RpcVersion, WireVersion,
    },
    scalars::{ArtifactRef, Counter, Id},
};
use serde::Serialize;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const MAX_INPUT: usize = 8 * 1024 * 1024;

type Analyze = fn(&[u8], &AtomicBool, u32) -> Result<Vec<u8>, Failure>;

pub struct Helper {
    pub package: &'static str,
    pub contribution: &'static str,
    pub format: IndexFormat,
    pub media_type: &'static str,
    pub analyze: Analyze,
}

type Callback = (String, mpsc::Sender<Option<RpcResult>>);
struct Channel {
    output: Mutex<io::Stdout>,
    waiting: Mutex<Option<Callback>>,
}
impl Channel {
    fn send(&self, message: &impl Serialize) -> Result<(), Failure> {
        let bytes = serde_json::to_vec(message).map_err(|_| "invalid-sdk-reply")?;
        let mut output = self.output.lock().map_err(|_| "protocol-unavailable")?;
        write_frame(&mut *output, &bytes).map_err(|_| "protocol-unavailable")?;
        output.flush().map_err(|_| "protocol-unavailable".into())
    }

    fn reply(&self, id: String, result: RpcResult) -> Result<(), Failure> {
        self.send(&RpcSuccess {
            jsonrpc: RpcVersion::V2,
            id,
            result,
        })
    }

    fn callback_reply(&self, id: &str, result: Option<RpcResult>) -> Result<(), Failure> {
        let mut waiting = self.waiting.lock().map_err(|_| "protocol-unavailable")?;
        let Some((expected, receiver)) = waiting.take() else {
            return Err("unsolicited-broker-reply".into());
        };
        if expected != id {
            return Err("foreign-broker-reply".into());
        }
        let _ = receiver.send(result);
        Ok(())
    }
}

struct Job {
    invoke: Invoke,
    request_id: String,
    deadline: Instant,
    cancelled: AtomicBool,
    finished: AtomicBool,
}
impl Job {
    fn ready(&self) -> Result<(), Failure> {
        if self.cancelled.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            return Err("cancelled".into());
        }
        Ok(())
    }

    fn outcome(&self, channel: &Channel, outcome: InvocationOutcome) -> Result<(), Failure> {
        channel.reply(
            self.request_id.clone(),
            RpcResult::InvocationFinished {
                invocation_id: self.invoke.invocation_id.clone(),
                outcome,
            },
        )
    }
}

struct Broker<'a> {
    job: &'a Job,
    channel: &'a Channel,
    sequence: &'a mut u64,
}
impl Broker<'_> {
    fn call(&mut self, call: BrokerCall) -> Result<RpcResult, Failure> {
        self.job.ready()?;
        *self.sequence += 1;
        let invocation = &self.job.invoke;
        let id = format!(
            "{}:p:{}",
            invocation.process_session_id.as_str(),
            self.sequence
        );
        let request = PackageRequest::Call {
            jsonrpc: RpcVersion::V2,
            id: id.clone(),
            params: BrokerInvoke {
                process_session_id: invocation.process_session_id.clone(),
                invocation_id: invocation.invocation_id.clone(),
                invocation_token: invocation.invocation_token.clone(),
                action_key: wire_id(format!("rust-index-{}", self.sequence))?,
                call,
            },
        };
        {
            let mut waiting = self
                .channel
                .waiting
                .lock()
                .map_err(|_| "protocol-unavailable")?;
            if waiting.is_some() {
                return Err("broker-reply-outstanding".into());
            }
            let (tx, rx) = mpsc::channel();
            *waiting = Some((id, tx));
            drop(waiting);
            self.channel.send(&request)?;
            loop {
                self.job.ready()?;
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(Some(result)) => return Ok(result),
                    Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err("artifact-broker-refused".into());
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        }
    }

    fn read(&mut self, artifact: &ArtifactRef) -> Result<Vec<u8>, Failure> {
        let declared = String::from(artifact.byte_length.clone())
            .parse::<usize>()
            .map_err(|_| "invalid-source-artifact")?;
        if declared > MAX_INPUT {
            return Err("source-budget".into());
        }
        let mut data = Vec::with_capacity(declared);
        loop {
            let RpcResult::ArtifactChunk {
                artifact: replied,
                offset,
                data: chunk,
                eof,
            } = self.call(BrokerCall::ArtifactRead {
                artifact: artifact.clone(),
                offset: Some(counter(data.len())?),
            })?
            else {
                return Err("invalid-source-artifact".into());
            };
            if serde_json::to_value(&replied).ok() != serde_json::to_value(artifact).ok()
                || String::from(offset) != data.len().to_string()
            {
                return Err("invalid-source-artifact".into());
            }
            let bytes = URL_SAFE_NO_PAD
                .decode(chunk)
                .map_err(|_| "invalid-source-artifact")?;
            if bytes.len() > ARTIFACT_CHUNK_BYTES
                || data.len().saturating_add(bytes.len()) > declared
                || bytes.is_empty() && !eof
            {
                return Err("invalid-source-artifact".into());
            }
            data.extend(bytes);
            if eof {
                if data.len() != declared
                    || sha256_hex(&data) != String::from(artifact.sha256.clone())
                {
                    return Err("invalid-source-artifact".into());
                }
                return Ok(data);
            }
        }
    }

    fn publish(&mut self, bytes: &[u8], media_type: &str) -> Result<ArtifactRef, Failure> {
        let write_id = wire_id("semantic-index")?;
        let mut total = 0;
        for chunk in bytes.chunks(ARTIFACT_CHUNK_BYTES) {
            total += chunk.len();
            let result = self.call(BrokerCall::ArtifactAppend {
                write_id: write_id.clone(),
                data: URL_SAFE_NO_PAD.encode(chunk),
            })?;
            if !matches!(result, RpcResult::ArtifactAppended { write_id: replied, byte_length }
                if replied.as_str() == write_id.as_str() && String::from(byte_length.clone()) == total.to_string())
            {
                return Err("semantic-artifact-write-refused".into());
            }
        }
        let sha256 = sha256_hex(bytes)
            .try_into()
            .map_err(|_| "semantic-artifact-write-refused")?;
        let RpcResult::BrokerCompleted { artifact } = self.call(BrokerCall::ArtifactPublish {
            write_id,
            media_type: media_type.into(),
            sha256,
        })?
        else {
            return Err("semantic-artifact-write-refused".into());
        };
        Ok(artifact)
    }
}

fn wire_id(value: impl Into<String>) -> Result<Id, Failure> {
    Id::try_from(value.into()).map_err(|_| "invalid-sdk-id".into())
}

fn counter(value: usize) -> Result<Counter, Failure> {
    value
        .to_string()
        .try_into()
        .map_err(|_| "invalid-sdk-counter".into())
}

fn work(
    helper: &Helper,
    channel: &Channel,
    job: &Job,
    sequence: &mut u64,
) -> Result<ArtifactRef, Failure> {
    let reference = job.invoke.input.clone();
    let mut broker = Broker {
        channel,
        job,
        sequence,
    };
    let source = broker.read(&reference)?;
    let remaining = job
        .deadline
        .saturating_duration_since(Instant::now())
        .as_millis();
    let output = (helper.analyze)(
        &source,
        &job.cancelled,
        remaining.min(u128::from(u32::MAX)) as u32,
    )?;
    let index = broker.publish(&output, helper.media_type)?;
    job.ready()?;
    channel.send(&PackageNotification::Event {
        jsonrpc: RpcVersion::V2,
        params: InvocationEvent {
            process_session_id: job.invoke.process_session_id.clone(),
            invocation_id: job.invoke.invocation_id.clone(),
            invocation_token: job.invoke.invocation_token.clone(),
            sequence: "1".to_owned().try_into().expect("positive sequence"),
            event: ContributionEvent::SemanticIndex {
                source_snapshot: reference,
                index: index.clone(),
                format: helper.format.clone(),
            },
        },
    })?;
    Ok(index)
}

fn worker(helper: Arc<Helper>, channel: Arc<Channel>, job: Arc<Job>, sequence: Arc<Mutex<u64>>) {
    let result = sequence
        .lock()
        .map_err(|_| Failure::from("protocol-unavailable"))
        .and_then(|mut sequence| work(&helper, &channel, &job, &mut sequence));
    let (outcome, terminate) = match result {
        Ok(index) => (
            InvocationOutcome::Completed {
                artifacts: vec![index],
            },
            false,
        ),
        Err(failure) if failure.code == "cancelled" => {
            (InvocationOutcome::CancelledBeforeEffect, true)
        }
        Err(failure) => (
            InvocationOutcome::Failed {
                code: wire_id(failure.code).expect("fixed fault code"),
            },
            failure.stop_group,
        ),
    };
    // Keep the job active until group teardown on failure. If the host closes its pipe while
    // the fault is being delivered, the protocol thread must also reap the owned group.
    if !terminate {
        job.finished.store(true, Ordering::Release);
    }
    let _ = job.outcome(&channel, outcome);
    if terminate {
        process::stop_group();
    }
}

#[derive(Default)]
struct State {
    session: Option<Id>,
    active: bool,
    job: Option<Arc<Job>>,
}
impl State {
    fn current(&self, id: &Id) -> Result<(), Failure> {
        if self
            .session
            .as_ref()
            .is_some_and(|current| current.as_str() == id.as_str())
        {
            Ok(())
        } else {
            Err("foreign-process-session".into())
        }
    }

    fn running(&self) -> Option<&Job> {
        self.job
            .as_deref()
            .filter(|job| !job.finished.load(Ordering::Acquire))
    }
}

// The worker reaps the compiler and unlinks its scratch files on cancellation. Keep a hard
// process-group stop as the bound if broker I/O or an uninterruptible child delays it.
fn stop_job(job: &Job) {
    job.cancelled.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !job.finished.load(Ordering::Acquire) {
        if Instant::now() >= deadline {
            process::stop_group();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn serve(helper: Arc<Helper>) -> Result<(), Failure> {
    let channel = Arc::new(Channel {
        output: Mutex::new(io::stdout()),
        waiting: Mutex::default(),
    });
    let sequence = Arc::new(Mutex::new(0));
    let mut input = io::stdin().lock();
    let mut state = State::default();
    loop {
        let frame = match read_frame(&mut input, MAX_FRAME_BYTES) {
            Ok(frame) => frame,
            Err(error) => {
                if let Some(job) = state.running() {
                    stop_job(job);
                }
                return match error {
                    FrameError::Closed => Ok(()),
                    _ => Err("host-stream-closed".into()),
                };
            }
        };
        let value = devworks_core::framing::parse(&frame).map_err(|_| "invalid-host-frame")?;
        if value.get("method").is_none() {
            if let Ok(success) = serde_json::from_value::<RpcSuccess>(value.clone()) {
                channel.callback_reply(&success.id, Some(success.result))?;
            } else {
                let failure: RpcFailure =
                    serde_json::from_value(value).map_err(|_| "invalid-broker-reply")?;
                let RpcFailureId::Known(id) = failure.id else {
                    return Err("invalid-broker-reply".into());
                };
                channel.callback_reply(&id, None)?;
            }
            continue;
        }
        let request: HostRequest =
            serde_json::from_value(value).map_err(|_| "invalid-host-request")?;
        request.validate().map_err(|_| "invalid-host-request")?;
        match request {
            HostRequest::Initialize { id, params, .. } => {
                if state.session.is_some()
                    || params.offered_min.major != 1
                    || params.offered_min.minor != 0
                    || params.offered_max.major != 1
                    || !params.required_features.is_empty()
                {
                    return Err("unsupported-sdk-protocol".into());
                }
                state.session = Some(params.process_session_id);
                channel.reply(
                    id,
                    RpcResult::Initialized {
                        selected: WireVersion { major: 1, minor: 0 },
                        manifest_digest: params.expected_manifest_digest,
                        contribution_ids: vec![
                            helper
                                .contribution
                                .to_owned()
                                .try_into()
                                .expect("namespaced contribution"),
                        ],
                        features: vec![],
                    },
                )?;
            }
            HostRequest::Activate { id, params, .. } => {
                state.current(&params.process_session_id)?;
                state.active = true;
                channel.reply(
                    id,
                    RpcResult::Activated {
                        registry_revision: params.registry_revision,
                        state_generation: params.state_generation,
                    },
                )?;
            }
            HostRequest::Invoke { id, params, .. } => {
                state.current(&params.process_session_id)?;
                if !state.active
                    || state.running().is_some()
                    || params.contribution_id.as_str() != helper.contribution
                {
                    return Err("invalid-semantic-invocation".into());
                }
                let job = Arc::new(Job {
                    deadline: Instant::now() + Duration::from_millis(u64::from(params.deadline_ms)),
                    invoke: params,
                    request_id: id,
                    cancelled: AtomicBool::new(false),
                    finished: AtomicBool::new(false),
                });
                state.job = Some(job.clone());
                let (channel, sequence) = (channel.clone(), sequence.clone());
                let helper = helper.clone();
                std::thread::spawn(move || worker(helper, channel, job, sequence));
            }
            HostRequest::Cancel { id, params, .. } => {
                state.current(&params.process_session_id)?;
                let invocation = params.invocation_id;
                channel.reply(
                    id,
                    RpcResult::CancelAcknowledged {
                        invocation_id: invocation.clone(),
                    },
                )?;
                if let Some(job) = state.running()
                    && invocation.as_str() == job.invoke.invocation_id.as_str()
                {
                    job.cancelled.store(true, Ordering::Release);
                }
            }
            HostRequest::Shutdown { id, params, .. } => {
                state.current(&params.process_session_id)?;
                channel.reply(id, RpcResult::ShutdownAcknowledged)?;
                if let Some(job) = state.running() {
                    stop_job(job);
                }
                return Ok(());
            }
        }
    }
}

pub fn run(helper: Helper) {
    let package = helper.package;
    if let Err(error) = serve(Arc::new(helper)) {
        eprintln!("{package}: {}", error.code);
        if process::owned_group() {
            process::stop_group();
        }
        std::process::exit(1);
    }
}
