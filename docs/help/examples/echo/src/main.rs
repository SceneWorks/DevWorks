//! A real protocol-v1 helper: no sockets, process launching, inference or host mutations.
use devworks_extension_protocol::{
    frame::{FrameError, MAX_FRAME_BYTES, read_frame, write_frame},
    package::{HostRequest, InvocationOutcome, RpcResult, RpcSuccess, RpcVersion, WireVersion},
};
use std::io;

#[derive(Default)]
struct Echo {
    process_session: Option<String>,
    active: bool,
}
impl Echo {
    fn same_session(&self, id: &str) -> Result<(), String> {
        if self.process_session.as_deref() == Some(id) {
            Ok(())
        } else {
            Err("foreign process session".into())
        }
    }
    fn respond(&mut self, request: HostRequest) -> Result<(RpcSuccess, bool), String> {
        request.validate().map_err(str::to_owned)?;
        let (id, result, stop) = match request {
            HostRequest::Initialize { id, params, .. } => {
                if self.process_session.is_some()
                    || params.offered_min.major != 1
                    || params.offered_max.major != 1
                    || params.offered_min.minor != 0
                    || !params.required_features.is_empty()
                {
                    return Err("unsupported or repeated initialization".into());
                }
                self.process_session = Some(params.process_session_id.as_str().into());
                (
                    id,
                    RpcResult::Initialized {
                        selected: WireVersion { major: 1, minor: 0 },
                        manifest_digest: params.expected_manifest_digest,
                        contribution_ids: vec![
                            "demo-echo/echo"
                                .to_owned()
                                .try_into()
                                .map_err(str::to_owned)?,
                        ],
                        features: vec![],
                    },
                    false,
                )
            }
            HostRequest::Activate { id, params, .. } => {
                self.same_session(params.process_session_id.as_str())?;
                self.active = true;
                (
                    id,
                    RpcResult::Activated {
                        registry_revision: params.registry_revision,
                        state_generation: params.state_generation,
                    },
                    false,
                )
            }
            HostRequest::Invoke { id, params, .. } => {
                self.same_session(params.process_session_id.as_str())?;
                if !self.active || params.contribution_id.as_str() != "demo-echo/echo" {
                    return Err("inactive or unknown contribution".into());
                }
                // The host already owns this exact input artifact. Echoing it does not
                // fabricate a new artifact or require an out-of-band file write.
                (
                    id,
                    RpcResult::InvocationFinished {
                        invocation_id: params.invocation_id,
                        outcome: InvocationOutcome::Completed {
                            artifacts: vec![params.input],
                        },
                    },
                    false,
                )
            }
            HostRequest::Cancel { id, params, .. } => {
                self.same_session(params.process_session_id.as_str())?;
                (
                    id,
                    RpcResult::CancelAcknowledged {
                        invocation_id: params.invocation_id,
                    },
                    false,
                )
            }
            HostRequest::Shutdown { id, params, .. } => {
                self.same_session(params.process_session_id.as_str())?;
                (id, RpcResult::ShutdownAcknowledged, true)
            }
        };
        Ok((
            RpcSuccess {
                jsonrpc: RpcVersion::V2,
                id,
                result,
            },
            stop,
        ))
    }
}
fn run() -> Result<(), String> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut echo = Echo::default();
    loop {
        let frame = match read_frame(&mut input, MAX_FRAME_BYTES) {
            Ok(frame) => frame,
            Err(FrameError::Closed) => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        let request: HostRequest =
            serde_json::from_slice(&frame).map_err(|_| "invalid host frame")?;
        let (reply, stop) = echo.respond(request)?;
        let bytes = serde_json::to_vec(&reply).map_err(|_| "invalid reply")?;
        write_frame(&mut output, &bytes).map_err(|error| error.to_string())?;
        if stop {
            return Ok(());
        }
    }
}
fn main() {
    if let Err(error) = run() {
        // Stdout must contain frames only; never print the input/token in a diagnostic.
        eprintln!("demo-echo: {error}");
        std::process::exit(1);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn request(value: serde_json::Value) -> HostRequest {
        serde_json::from_value(value).unwrap()
    }
    #[test]
    fn handshake_and_activation_use_the_actual_host_binding() {
        let mut echo = Echo::default();
        let (initialized, _) = echo.respond(request(json!({"jsonrpc":"2.0","id":"init","method":"host.initialize","params":{"processSessionId":"session","offeredMin":{"major":1,"minor":0},"offeredMax":{"major":1,"minor":0},"expectedManifestDigest":"a".repeat(64),"requiredFeatures":[],"maxFrameBytes":1048576}}))).unwrap();
        assert_eq!(initialized.id, "init");
        let active = |session| {
            request(
                json!({"jsonrpc":"2.0","id":"activate","method":"host.activate","params":{"processSessionId":session,"registryRevision":"2","stateGeneration":"3","configurationRevision":"1"}}),
            )
        };
        assert!(echo.respond(active("foreign")).is_err());
        let (activated, _) = echo.respond(active("session")).unwrap();
        assert!(
            matches!(activated.result,RpcResult::Activated{registry_revision,..} if String::from(registry_revision.clone())=="2")
        );
        let (done,_) = echo.respond(request(json!({"jsonrpc":"2.0","id":"invoke","method":"host.invoke","params":{"processSessionId":"session","invocationId":"invocation","invocationToken":"token","contributionId":"demo-echo/echo","scope":{"kind":"Workspace"},"allowedOperations":[],"registryRevision":"2","stateGeneration":"3","authorityRevision":"1","controlGeneration":"1","operationId":"operation","deadlineMs":1000,"input":{"artifactId":"input","versionId":"version","sha256":"b".repeat(64),"byteLength":"2","mediaType":"application/json"}}}))).unwrap();
        let RpcResult::InvocationFinished {
            outcome: InvocationOutcome::Completed { artifacts },
            ..
        } = done.result
        else {
            panic!("expected completion")
        };
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].artifact_id.as_str(), "input");
    }
    #[test]
    fn documented_contribution_and_event_shapes_decode_with_the_actual_sdk() {
        let contributions: Vec<devworks_extension_protocol::package::Contribution> =
            serde_json::from_str(include_str!("../contributions.json")).unwrap();
        assert_eq!(contributions.len(), 8);
        let event: devworks_extension_protocol::package::PackageRequest =
            serde_json::from_str(include_str!("../event-commit.json")).unwrap();
        event.validate().unwrap();
    }
}
