# Complete native echo example source

This is the full source of the tested local development example. It is bundled with the app so the source remains readable offline. Building requires a matching DevWorks source checkout, Rust toolchain, and the path dependencies below; Cargo may need cached dependencies or a network during setup. It does not install itself or bypass human package approval.

See [Build and test](help:sdk-start) for commands and [Process protocol](help:sdk-protocol) for framing and lifecycle. Create the files at the shown paths under `docs/help/examples/echo` in a matching source checkout.

## Cargo.toml

```toml
[package]
name = "devworks-help-echo-example"
version = "1.0.0"
edition = "2024"
publish = false

# A standalone developer example, not another application/runtime workspace member.
[workspace]

[dependencies]
devworks-extension-protocol = { path = "../../../../crates/devworks-extension-protocol" }
devworks-core = { path = "../../../../crates/devworks-core" }
devworks-adapters = { path = "../../../../crates/devworks-adapters" }
serde_json = "=1.0.154"
sha2 = "=0.11.1"

[[bin]]
name = "demo-echo"
path = "src/main.rs"

[[bin]]
name = "build-bundle"
path = "src/bin/build_bundle.rs"
```

## src/main.rs

```rust
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
```

## src/bin/build_bundle.rs

```rust
//! Build an inspected unsigned development bundle using the actual native bundle owner.
use devworks_adapters::packages::{BundleInspector, host_target_triple, write_bundle};
use devworks_core::{fingerprint::sha256_hex, ports::packages::PackageInspector};
use serde_json::{Value, json};
use std::path::Path;
fn build(helper: &[u8]) -> Result<Vec<u8>, String> {
    let data = json!({"type":"object"});
    let config = json!({"type":"object","properties":{},"additionalProperties":false});
    let state = config.clone();
    let schema_ref = |name: &str, schema: &Value| {
        json!({
            "schemaId":format!("demo-echo/{name}"),"revision":"1",
            "sha256":sha256_hex(devworks_core::jcs::canonicalize(schema).unwrap().as_bytes())
        })
    };
    let data_ref = schema_ref("data", &data);
    let config_ref = schema_ref("config", &config);
    let state_ref = schema_ref("state", &state);
    let entrypoint = if cfg!(windows) {
        "bin/demo-echo.exe"
    } else {
        "bin/demo-echo"
    };
    let files = [
        (entrypoint, helper.to_vec()),
        (
            "help.md",
            b"# Echo\nReturns the exact input artifact. No host mutation or inference.\n".to_vec(),
        ),
        ("schemas/data.json", serde_json::to_vec(&data).unwrap()),
        ("schemas/config.json", serde_json::to_vec(&config).unwrap()),
        ("schemas/state.json", serde_json::to_vec(&state).unwrap()),
    ];
    let manifest = json!({
        "packageId":"demo-echo","version":"1.0.0","publisherId":"local-development",
        "protocolMin":{"major":1,"minor":0},"protocolMax":{"major":1,"minor":0},
        "minimumHostVersion":"0.1.0","dependencies":[],"runtimes":[],
        "schemaAssets":[{"reference":data_ref,"path":"schemas/data.json"},{"reference":config_ref,"path":"schemas/config.json"},{"reference":state_ref,"path":"schemas/state.json"}],
        "requestedLimits":{"maxConcurrent":1,"queueCapacity":4,"maxMemoryBytes":"67108864","streamCreditBytes":4096},
        "targets":[{"targetTriple":host_target_triple(),"entrypoint":entrypoint}],
        "files":files.iter().map(|(path,bytes)|json!({"path":path,"sha256":sha256_hex(bytes),"byteLength":bytes.len().to_string()})).collect::<Vec<_>>(),
        "stateSchema":state_ref,"configurationSchema":config_ref,"helpFile":"help.md",
        "requestedPermissions":[],
        "contributions":[{"kind":"Tool","meta":{"id":"demo-echo/echo","inputSchema":data_ref,"outputSchema":data_ref,"helpTopic":"echo","required":true,"cancellation":"Cooperative"},"requestedEffect":"ReadCandidate"}]
    });
    let borrowed = files
        .iter()
        .map(|(path, bytes)| (*path, bytes.as_slice()))
        .collect::<Vec<_>>();
    let bundle = write_bundle(&serde_json::to_vec(&manifest).unwrap(), &borrowed, None);
    BundleInspector
        .inspect(&bundle)
        .map_err(|error| format!("native bundle inspection refused: {error:?}"))?;
    Ok(bundle)
}
fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 2 {
        eprintln!("usage: build-bundle <native-helper-path> <output.dwpkg>");
        std::process::exit(2);
    }
    let result = std::fs::read(Path::new(&args[0]))
        .map_err(|error| error.to_string())
        .and_then(|helper| build(&helper))
        .and_then(|bundle| std::fs::write(&args[1], bundle).map_err(|error| error.to_string()));
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
    println!(
        "Built {}. Import and inspect it before exact human approval.",
        args[1]
    );
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_files_schemas_and_help_pass_the_real_native_inspector() {
        let bundle = build(b"development helper fixture").unwrap();
        let inspected = BundleInspector.inspect(&bundle).unwrap();
        assert_eq!(inspected.inspection.package_id, "demo-echo");
        assert!(inspected.inspection.signer_key.is_none());
        assert_eq!(inspected.files.len(), 5);
    }
}
```

## contributions.json

```json
[
  {
    "kind": "Tool",
    "meta": {
      "id": "demo-echo/echo",
      "inputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "outputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "helpTopic": "echo",
      "required": false,
      "cancellation": "Cooperative"
    },
    "requestedEffect": "ReadCandidate"
  },
  {
    "kind": "Semantic",
    "meta": {
      "id": "demo-echo/index",
      "inputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "outputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "helpTopic": "echo",
      "required": false,
      "cancellation": "Cooperative"
    },
    "languages": [
      "csharp"
    ],
    "format": "Scip"
  },
  {
    "kind": "Workflow",
    "meta": {
      "id": "demo-echo/verify",
      "inputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "outputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "helpTopic": "echo",
      "required": false,
      "cancellation": "Cooperative"
    },
    "role": "VerificationGate",
    "evidenceSchema": {
      "schemaId": "demo-echo/data",
      "revision": "1",
      "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }
  },
  {
    "kind": "EventHandler",
    "meta": {
      "id": "demo-echo/on-event",
      "inputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "outputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "helpTopic": "echo",
      "required": false,
      "cancellation": "Cooperative"
    },
    "eventSchemas": [
      {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      }
    ]
  },
  {
    "kind": "ContextHook",
    "meta": {
      "id": "demo-echo/assemble",
      "inputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "outputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "helpTopic": "echo",
      "required": false,
      "cancellation": "Cooperative"
    },
    "phase": "Assemble",
    "priority": 10
  },
  {
    "kind": "Content",
    "meta": {
      "id": "demo-echo/skill",
      "inputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "outputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "helpTopic": "echo",
      "required": false,
      "cancellation": "Cooperative"
    },
    "contentKind": "Skill",
    "artifact": {
      "artifactId": "artifact",
      "versionId": "version",
      "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "byteLength": "2",
      "mediaType": "application/json"
    }
  },
  {
    "kind": "Provider",
    "meta": {
      "id": "demo-echo/provider",
      "inputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "outputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "helpTopic": "echo",
      "required": false,
      "cancellation": "Cooperative"
    },
    "profiles": [
      "CompatibleChat"
    ],
    "endpointIds": [
      "local"
    ]
  },
  {
    "kind": "NativeUi",
    "meta": {
      "id": "demo-echo/view",
      "inputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "outputSchema": {
        "schemaId": "demo-echo/data",
        "revision": "1",
        "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      },
      "helpTopic": "echo",
      "required": false,
      "cancellation": "Cooperative"
    },
    "view": {
      "kind": "Group",
      "children": [
        {
          "kind": "Text",
          "text": "Index status"
        },
        {
          "kind": "Action",
          "actionId": "demo-echo/echo",
          "label": "Inspect"
        }
      ]
    }
  }
]
```

## event-commit.json

```json
{
  "jsonrpc": "2.0",
  "id": "event-commit",
  "method": "broker.commit_event",
  "params": {
    "processSessionId": "actual-session",
    "invocationId": "actual-invocation",
    "invocationToken": "actual-token",
    "subscriptionId": "actual-subscription",
    "eventId": "actual-event",
    "expectedCheckpoint": "0",
    "stateGeneration": "1",
    "expectedStateRevision": "1",
    "intents": []
  }
}
```

The contribution and event JSON files are typed contract examples, not a complete installable package. Their illustrative digests, tokens, IDs, endpoint and resource declarations must come from the actual host/package publication when used. The echo bundle builder above creates its own real file and schema digests.
