//! Test-only package helper that speaks the DevWorks package protocol (framed JSON-RPC over
//! stdin/stdout). The engine's integration tests install it inside signed fixture bundles; it
//! is not a product package and is never installed by the application itself.
//!
//! Behaviour comes from `fixture.json` in its working directory (the install directory, so it
//! is part of the verified, digest-bound package contents):
//!
//! * `mode`: `normal`, `hang-initialize` (never answers), `wrong-digest`, `wrong-inventory`,
//!   `exit-on-activate` (exits 3), `exit-after-ready` (exits 4 shortly after activation) or
//!   `deaf-after-ready` (stops reading its input after activation, without exiting) or
//!   `slow-handshake` (answers `host.initialize` and `host.activate` after 1 s each) or
//!   `slow-initialize` (answers `host.initialize` after 1.2 s);
//! * `inventory`: the contribution IDs it reports.
//!
//! On `host.shutdown` it writes the deadline it was given to
//! `devworks-fixture-<package>-shutdown` in the temporary directory while
//! `devworks-fixture-<package>-shutdown-watch` exists there, then exits.
//!
//! In any mode it exits 3 on activation while `devworks-fixture-<package>-fail-on-activate`
//! exists in the temporary directory (a test makes an installed version fail later).
//!
//! Each invoked contribution's local name selects what the invocation does: `echo` (progress,
//! then completes with its input), `sleep` (completes after 300 ms), `wait` (until cancelled,
//! then `CancelledBeforeEffect`), `stubborn` (acknowledges cancellation but never finishes),
//! `state` (broker.state read then compare-and-set, results as progress), `event` (tries
//! broker.state compare-and-set and an HTTP broker.call, then broker.commit_event with the
//! commit read from `devworks-fixture-<package>-commit.json` in the temporary directory),
//! `react` (an event handler: reads its event, asks for the inference
//! `devworks-fixture-<package>-react.json` names, commits the event and reports each step),
//! `config` (reports the configuration `host.activate` delivered as progress `config:<json>`,
//! or `config:none`), `late-state` (reads its state, answers the invocation, then tries a
//! compare-and-set under the finished invocation's token and writes the answer's class to
//! `devworks-fixture-<package>-late-state` in the temporary directory), `crash` (exits 5
//! mid-invocation) and `call` (makes each `broker.call` of `calls` in
//! `devworks-fixture-<package>-call.json` in the temporary directory, `"$input"` naming its input,
//! `"$last"` the previous result and `"$lastData"` the data of the last chunk read, reports `call:<actionKey>:<class>` as progress (followed by
//! `:<error message>` when the step sets `reportMessage`; with `dumpEnv` it first writes its
//! environment to `devworks-fixture-<package>-env` in the temporary directory) and completes
//! with the results, plus a forged artifact when `forge` and its input when `returnInput`).
use devworks_extension_protocol::frame::{MAX_FRAME_BYTES, read_frame, write_frame};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Out {
    w: Mutex<std::io::Stdout>,
}
impl Out {
    fn send(&self, v: &Value) {
        let mut w = self.w.lock().unwrap();
        let _ = write_frame(&mut *w, v.to_string().as_bytes());
        let _ = w.flush();
    }
}

struct State {
    out: Arc<Out>,
    session: Mutex<String>,
    next: AtomicU64,
    /// Our outstanding broker requests by ID.
    waiting: Mutex<HashMap<String, mpsc::Sender<Value>>>,
    /// Cancellation signals by invocation ID.
    cancels: Mutex<HashMap<String, mpsc::Sender<()>>>,
    /// The configuration the host delivered in activation (`null` when none).
    configuration: Mutex<Value>,
}
impl State {
    fn call(&self, method: &str, params: Value) -> Value {
        let id = format!(
            "{}:p:{}",
            self.session.lock().unwrap(),
            self.next.fetch_add(1, Ordering::SeqCst) + 1
        );
        let (tx, rx) = mpsc::channel();
        self.waiting.lock().unwrap().insert(id.clone(), tx);
        self.out
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx.recv_timeout(Duration::from_secs(10))
            .unwrap_or(json!({"error": {"data": {"class": "Timeout"}}}))
    }
    fn progress(&self, inv: &Value, n: u64, text: &str) {
        self.out.send(
            &json!({"jsonrpc": "2.0", "method": "package.progress", "params": {
            "processSessionId": inv["processSessionId"], "invocationId": inv["invocationId"],
            "invocationToken": inv["invocationToken"], "sequence": n.to_string(), "text": text,
            "droppedCount": "0"}}),
        );
    }
}

fn class(reply: &Value) -> String {
    if let Some(c) = reply["error"]["data"]["class"].as_str() {
        return c.to_owned();
    }
    reply["result"]["kind"]
        .as_str()
        .unwrap_or("none")
        .to_owned()
}

fn result(id: &Value, r: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": r})
}

fn finished(st: &State, id: &Value, inv: &Value, outcome: Value) {
    st.out.send(&result(
        id,
        json!({"kind": "InvocationFinished", "invocationId": inv["invocationId"], "outcome": outcome}),
    ));
}

fn invoke(st: Arc<State>, id: Value, inv: Value, cancel: mpsc::Receiver<()>, package: String) {
    let contribution = inv["contributionId"].as_str().unwrap_or("").to_owned();
    let name = contribution.split('/').nth(1).unwrap_or("");
    match name {
        "echo" => {
            st.progress(&inv, 1, "working");
            finished(
                &st,
                &id,
                &inv,
                json!({"kind": "Completed", "artifacts": [inv["input"]]}),
            );
        }
        "sleep" => {
            std::thread::sleep(Duration::from_millis(300));
            finished(
                &st,
                &id,
                &inv,
                json!({"kind": "Completed", "artifacts": []}),
            );
        }
        "wait" => {
            let _ = cancel.recv();
            finished(&st, &id, &inv, json!({"kind": "CancelledBeforeEffect"}));
        }
        "stubborn" => {
            let _ = cancel.recv();
            std::thread::sleep(Duration::from_secs(3600));
        }
        "queued-state" => {
            let root = std::env::temp_dir();
            let marker = |suffix: &str| root.join(format!("devworks-fixture-{package}-{suffix}"));
            let _ = std::fs::write(marker("cas-ready"), "ready");
            let until = std::time::Instant::now() + Duration::from_secs(10);
            while !marker("cas-go").exists() && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            let params = json!({
                "processSessionId": inv["processSessionId"],
                "invocationId": inv["invocationId"],
                "invocationToken": inv["invocationToken"],
                "stateGeneration": inv["stateGeneration"],
                "actionKey": "queued-cas",
                "action": {
                    "kind": "CompareAndSet",
                    "expectedRevision": "1",
                    "proposal": inv["input"]
                }
            });
            let child = st.clone();
            let result_path = marker("cas-result");
            let worker = std::thread::spawn(move || {
                let result = child.call("broker.state", params);
                let _ = std::fs::write(result_path, class(&result));
            });
            while !marker("cas-finish").exists() && std::time::Instant::now() < until {
                if cancel.try_recv().is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            finished(&st, &id, &inv, json!({"kind":"Completed","artifacts":[]}));
            let _ = worker.join();
        }
        "late-state" => {
            let base = json!({"processSessionId": inv["processSessionId"], "invocationId": inv["invocationId"],
                "invocationToken": inv["invocationToken"], "stateGeneration": inv["stateGeneration"]});
            let mut read = base.clone();
            read["actionKey"] = json!("read-1");
            read["action"] = json!({"kind": "Read"});
            let snap = st.call("broker.state", read);
            finished(
                &st,
                &id,
                &inv,
                json!({"kind": "Completed", "artifacts": []}),
            );
            let mut cas = base;
            cas["actionKey"] = json!("cas-late");
            cas["action"] = json!({"kind": "CompareAndSet",
                "expectedRevision": snap["result"]["revision"].as_str().unwrap_or("1"), "proposal": inv["input"]});
            let late = st.call("broker.state", cas);
            let _ = std::fs::write(
                std::env::temp_dir().join(format!("devworks-fixture-{package}-late-state")),
                class(&late),
            );
        }
        "config" => {
            let c = st.configuration.lock().unwrap().clone();
            let text = if c.is_null() {
                "config:none".to_owned()
            } else {
                format!("config:{c}")
            };
            st.progress(&inv, 1, &text);
            finished(
                &st,
                &id,
                &inv,
                json!({"kind": "Completed", "artifacts": []}),
            );
        }
        "crash" => {
            st.progress(&inv, 1, "about to crash");
            std::thread::sleep(Duration::from_millis(50));
            std::process::exit(5);
        }
        "state" => {
            let base = json!({"processSessionId": inv["processSessionId"], "invocationId": inv["invocationId"],
                "invocationToken": inv["invocationToken"], "stateGeneration": inv["stateGeneration"]});
            let mut read = base.clone();
            read["actionKey"] = json!("read-1");
            read["action"] = json!({"kind": "Read"});
            let snap = st.call("broker.state", read);
            st.progress(
                &inv,
                1,
                &format!(
                    "read:{}:{}",
                    class(&snap),
                    snap["result"]["revision"].as_str().unwrap_or("-")
                ),
            );
            let mut cas = base.clone();
            cas["actionKey"] = json!("cas-1");
            cas["action"] = json!({"kind": "CompareAndSet",
                "expectedRevision": snap["result"]["revision"].as_str().unwrap_or("1"), "proposal": inv["input"]});
            let done = st.call("broker.state", cas);
            st.progress(
                &inv,
                2,
                &format!(
                    "cas:{}:{}",
                    class(&done),
                    done["result"]["revision"].as_str().unwrap_or("-")
                ),
            );
            // A stale token is refused.
            let mut forged = base;
            forged["invocationToken"] = json!("forged-token");
            forged["actionKey"] = json!("read-2");
            forged["action"] = json!({"kind": "Read"});
            let refused = st.call("broker.state", forged);
            st.progress(&inv, 3, &format!("forged:{}", class(&refused)));
            finished(
                &st,
                &id,
                &inv,
                json!({"kind": "Completed", "artifacts": []}),
            );
        }
        "event" => {
            let ids = json!({"processSessionId": inv["processSessionId"], "invocationId": inv["invocationId"],
                "invocationToken": inv["invocationToken"]});
            let mut cas = ids.clone();
            cas["actionKey"] = json!("cas-1");
            cas["stateGeneration"] = inv["stateGeneration"].clone();
            cas["action"] =
                json!({"kind": "CompareAndSet", "expectedRevision": "1", "proposal": inv["input"]});
            let r = st.call("broker.state", cas);
            st.progress(&inv, 1, &format!("cas:{}", class(&r)));
            let mut call = ids.clone();
            call["actionKey"] = json!("http-1");
            call["call"] = json!({"kind": "Http", "connectionHandle": "c1", "endpointId": "e1",
                "httpMethod": "Post", "relativePath": "/notify"});
            let r = st.call("broker.call", call);
            st.progress(&inv, 2, &format!("call:{}", class(&r)));
            let path = std::env::temp_dir().join(format!("devworks-fixture-{package}-commit.json"));
            let commit: Value = std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or(json!({}));
            let echo = |sequence: u64, event_id: &Value, receipt: &str| {
                let mut params = ids.clone();
                params["sequence"] = json!(sequence.to_string());
                params["event"] = json!({
                    "kind": "EventProcessed", "subscriptionId": commit["subscriptionId"],
                    "eventId": event_id, "processingReceipt": receipt
                });
                st.out.send(&json!({
                    "jsonrpc": "2.0", "method": "package.event", "params": params
                }));
            };
            echo(1, &commit["eventId"], "unverified-receipt");
            if commit["echoOnly"] == true {
                finished(
                    &st,
                    &id,
                    &inv,
                    json!({"kind": "Completed", "artifacts": []}),
                );
                return;
            }
            let mut params = ids.clone();
            for (k, v) in commit.as_object().cloned().unwrap_or_default() {
                params[k] = if v == "$input" {
                    inv["input"].clone()
                } else {
                    v
                };
            }
            let r = st.call("broker.commit_event", params.clone());
            st.progress(
                &inv,
                3,
                &format!(
                    "commit:{}:{}",
                    class(&r),
                    r["result"]["stateRevision"].as_str().unwrap_or("-")
                ),
            );
            if let Some(receipt) = r["result"]["processingReceipt"].as_str() {
                echo(2, &json!("other-event"), receipt);
                echo(3, &commit["eventId"], "another-unverified-receipt");
                echo(4, &commit["eventId"], receipt);
                echo(5, &commit["eventId"], receipt);
            }
            // An identical commit replays its receipt.
            let again = st.call("broker.commit_event", params);
            st.progress(
                &inv,
                4,
                &format!(
                    "replay:{}:{}",
                    class(&again),
                    (again["result"]["processingReceipt"] == r["result"]["processingReceipt"])
                ),
            );
            finished(
                &st,
                &id,
                &inv,
                json!({"kind": "Completed", "artifacts": []}),
            );
        }
        "react" => {
            // An event handler fired by a host event: reads its event, asks for inference with
            // the request `devworks-fixture-<package>-react.json` names (`request`: the request
            // JSON, `inputSchema`: its schema reference), then commits the event.
            let ids = json!({"processSessionId": inv["processSessionId"], "invocationId": inv["invocationId"],
                "invocationToken": inv["invocationToken"]});
            let script: Value = std::fs::read(
                std::env::temp_dir().join(format!("devworks-fixture-{package}-react.json")),
            )
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(json!({}));
            let broker_call = |key: &str, call: Value| {
                let mut params = ids.clone();
                params["actionKey"] = json!(key);
                params["call"] = call;
                st.call("broker.call", params)
            };
            let mut read = ids.clone();
            read["actionKey"] = json!("state-read");
            read["stateGeneration"] = inv["stateGeneration"].clone();
            read["action"] = json!({"kind": "Read"});
            let snap = st.call("broker.state", read);
            let chunk = broker_call(
                "read-event",
                json!({"kind": "ArtifactRead", "artifact": inv["input"]}),
            );
            let event: Value = chunk["result"]["data"]
                .as_str()
                .and_then(base64url_decode)
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or(json!({}));
            st.progress(&inv, 1, &format!("event:{}", class(&chunk)));
            if script["request"].is_object() {
                let bytes = script["request"].to_string().into_bytes();
                let appended = broker_call(
                    "append",
                    json!({"kind": "ArtifactAppend", "writeId": "request",
                        "data": base64url(&bytes)}),
                );
                let published = broker_call(
                    "publish",
                    json!({"kind": "ArtifactPublish", "writeId": "request",
                        "mediaType": "application/json",
                        "sha256": sha256_hex(&bytes)}),
                );
                st.progress(
                    &inv,
                    2,
                    &format!("request:{}:{}", class(&appended), class(&published)),
                );
                let answer = broker_call(
                    "infer",
                    json!({"kind": "Inference", "inputSchema": script["inputSchema"],
                        "request": published["result"]["artifact"]}),
                );
                st.progress(
                    &inv,
                    3,
                    &format!(
                        "infer:{}:{}",
                        class(&answer),
                        answer["error"]["message"].as_str().unwrap_or("")
                    ),
                );
            }
            let mut commit = ids.clone();
            commit["subscriptionId"] = event["subscriptionId"].clone();
            commit["eventId"] = event["eventId"].clone();
            commit["expectedCheckpoint"] = event["expectedCheckpoint"].clone();
            commit["stateGeneration"] = inv["stateGeneration"].clone();
            commit["expectedStateRevision"] = snap["result"]["revision"].clone();
            commit["intents"] = json!([]);
            let source = &event["event"]["event"];
            if let Some(intent) = script["applicationIntent"].as_object()
                && intent["triggerPackage"] == source["packageId"]
                && let Some(revision) = source["registryRevision"].as_i64()
            {
                let arguments = json!({"command": {"kind": "Disable",
                    "packageId": intent["targetPackage"], "expectedRegistryRevision": revision.to_string()}});
                let bytes = arguments.to_string().into_bytes();
                broker_call(
                    "append-intent",
                    json!({"kind": "ArtifactAppend", "writeId": "intent", "data": base64url(&bytes)}),
                );
                let published = broker_call(
                    "publish-intent",
                    json!({"kind": "ArtifactPublish", "writeId": "intent",
                        "mediaType": "application/json", "sha256": sha256_hex(&bytes)}),
                );
                commit["intents"] = json!([{"actionKey": "app-intent", "call": {
                    "kind": "Application", "operation": intent["operation"],
                    "inputSchema": intent["inputSchema"], "arguments": published["result"]["artifact"]
                }}]);
            }
            let committed = st.call("broker.commit_event", commit);
            st.progress(&inv, 4, &format!("commit:{}", class(&committed)));
            finished(
                &st,
                &id,
                &inv,
                json!({"kind": "Completed", "artifacts": []}),
            );
        }
        "native-view" => {
            let binding = json!({"processSessionId": inv["processSessionId"], "invocationId": inv["invocationId"],
                "invocationToken": inv["invocationToken"]});
            let call = |action: &str, data: Value| {
                let mut params = binding.clone();
                params["actionKey"] = json!(action);
                params["call"] = data;
                st.call("broker.call", params)
            };
            let content =
                json!({"tables": { (format!("{package}/view-rows")): [{"result":"Available"}] }})
                    .to_string();
            let appended = call(
                "view-append",
                json!({"kind": "ArtifactAppend", "writeId":"view",
                "data":base64url(content.as_bytes())}),
            );
            let published = call(
                "view-publish",
                json!({"kind":"ArtifactPublish", "writeId":"view",
                "mediaType":"application/json", "sha256":sha256_hex(content.as_bytes())}),
            );
            if class(&appended) != "ArtifactAppended" || class(&published) != "BrokerCompleted" {
                finished(
                    &st,
                    &id,
                    &inv,
                    json!({"kind":"Failed","code":"ViewPublicationFailed"}),
                );
                return;
            }
            let mut params = binding;
            params["sequence"] = json!("1");
            params["event"] =
                json!({"kind":"ViewUpdate","revision":"1","data":published["result"]["artifact"]});
            st.out
                .send(&json!({"jsonrpc":"2.0","method":"package.event","params":params}));
            finished(&st, &id, &inv, json!({"kind":"Completed","artifacts":[]}));
        }
        "call" => {
            let path = std::env::temp_dir().join(format!("devworks-fixture-{package}-call.json"));
            let script: Value = std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or(json!({}));
            if script["dumpEnv"] == json!(true) {
                // What the host gave this process: its environment, for a test to inspect.
                let env: String = std::env::vars()
                    .map(|(k, v)| format!("{k}={v}\n"))
                    .collect();
                let dump = std::env::temp_dir().join(format!("devworks-fixture-{package}-env"));
                let _ = std::fs::write(dump, env);
            }
            let mut produced = Vec::new();
            let mut last_data = Value::Null;
            let calls = script["calls"].as_array().cloned().unwrap_or_default();
            for (n, step) in calls.iter().enumerate() {
                if let Some(ms) = step["sleepMs"].as_u64() {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                }
                if let Some(grant) = step["credit"].as_object() {
                    let stream_id = grant.get("streamId").unwrap_or(&inv["input"]["versionId"]);
                    let token = grant.get("token").unwrap_or(&inv["invocationToken"]);
                    let process = grant
                        .get("processSessionId")
                        .unwrap_or(&inv["processSessionId"]);
                    st.out.send(&json!({"jsonrpc": "2.0", "method": "broker.streamCredit", "params": {
                        "processSessionId": process, "invocationId": inv["invocationId"],
                        "invocationToken": token, "streamId": stream_id, "sequence": grant["sequence"],
                        "grantBytes": grant["bytes"]
                    }}));
                }
                let mut call = step["call"].clone();
                // `"$input"` names this invocation's input; `"$last"` the previous result.
                for (k, v) in call.as_object().cloned().unwrap_or_default() {
                    if v == json!("$input") {
                        call[&k] = inv["input"].clone();
                    } else if v == json!("$last") {
                        call[&k] = produced.last().cloned().unwrap_or(Value::Null);
                    } else if v == json!("$lastData") {
                        call[&k] = last_data.clone();
                    }
                }
                let params = json!({"processSessionId": inv["processSessionId"],
                    "invocationId": inv["invocationId"], "invocationToken": inv["invocationToken"],
                    "actionKey": step["actionKey"], "call": call});
                let r = st.call("broker.call", params);
                let key = step["actionKey"].as_str().unwrap_or("-");
                let mut text = format!("call:{key}:{}", class(&r));
                if step["reportMessage"] == json!(true) {
                    text.push(':');
                    text.push_str(r["error"]["message"].as_str().unwrap_or(""));
                }
                if step["reportBytes"] == true
                    && let Some(bytes) = r["result"]["data"].as_str().and_then(base64url_decode)
                {
                    text.push_str(&format!(":{}", bytes.len()));
                }
                st.progress(&inv, n as u64 + 1, &text);
                if let Some(d) = r["result"].get("data") {
                    last_data = d.clone();
                } else if let Some(a) = r["result"].get("artifact") {
                    produced.push(a.clone());
                }
            }
            if script["forge"] == json!(true) {
                produced.push(json!({"artifactId": "forged", "versionId": "forged",
                    "sha256": "f".repeat(64), "byteLength": "1", "mediaType": "application/json"}));
            }
            if script["returnInput"] == json!(true) {
                produced.push(inv["input"].clone());
            }
            finished(
                &st,
                &id,
                &inv,
                json!({"kind": "Completed", "artifacts": produced}),
            );
        }
        _ => finished(
            &st,
            &id,
            &inv,
            json!({"kind": "Failed", "code": "UnknownContribution"}),
        ),
    }
}

fn main() {
    let config: Value = std::fs::read("fixture.json")
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(json!({}));
    let mode = config["mode"].as_str().unwrap_or("normal").to_owned();
    let mut inventory: Vec<Value> = config["inventory"].as_array().cloned().unwrap_or_default();
    if mode == "wrong-inventory" {
        inventory.pop();
    }
    let package = std::env::var("DEVWORKS_PACKAGE_ID").unwrap_or_default();
    let st = Arc::new(State {
        out: Arc::new(Out {
            w: Mutex::new(std::io::stdout()),
        }),
        session: Mutex::new(String::new()),
        next: AtomicU64::new(0),
        waiting: Mutex::new(HashMap::new()),
        cancels: Mutex::new(HashMap::new()),
        configuration: Mutex::new(Value::Null),
    });
    eprintln!("fixture helper started in mode {mode}");
    let mut stdin = std::io::stdin();
    loop {
        let Ok(bytes) = read_frame(&mut stdin, MAX_FRAME_BYTES) else {
            return;
        };
        let Ok(msg) = serde_json::from_slice::<Value>(&bytes) else {
            return;
        };
        let id = msg["id"].clone();
        match msg["method"].as_str() {
            None => {
                // An answer to one of our broker requests.
                if let Some(tx) = id
                    .as_str()
                    .and_then(|i| st.waiting.lock().unwrap().remove(i))
                {
                    let _ = tx.send(msg);
                }
            }
            Some("host.initialize") => {
                if mode == "hang-initialize" {
                    continue;
                }
                if mode == "slow-handshake" {
                    std::thread::sleep(Duration::from_secs(1));
                }
                if mode == "slow-initialize" {
                    std::thread::sleep(Duration::from_millis(1200));
                }
                let p = &msg["params"];
                *st.session.lock().unwrap() =
                    p["processSessionId"].as_str().unwrap_or("").to_owned();
                let digest = if mode == "wrong-digest" {
                    json!("f".repeat(64))
                } else {
                    p["expectedManifestDigest"].clone()
                };
                let features = if mode == "credited-artifacts" {
                    vec![devworks_extension_protocol::package::ARTIFACT_READ_CREDIT]
                } else {
                    Vec::new()
                };
                st.out.send(&result(
                    &id,
                    json!({"kind": "Initialized", "selected": p["offeredMax"],
                    "manifestDigest": digest, "contributionIds": inventory, "features": features}),
                ));
            }
            Some("host.activate") => {
                let marked = std::env::temp_dir()
                    .join(format!("devworks-fixture-{package}-fail-on-activate"))
                    .exists();
                if mode == "slow-handshake" {
                    std::thread::sleep(Duration::from_secs(1));
                }
                if mode == "exit-on-activate" || marked {
                    eprintln!("fixture helper exits on activation");
                    std::process::exit(3);
                }
                let p = &msg["params"];
                *st.configuration.lock().unwrap() = p["configuration"].clone();
                st.out.send(&result(&id, json!({"kind": "Activated",
                    "registryRevision": p["registryRevision"], "stateGeneration": p["stateGeneration"]})));
                if mode == "exit-after-ready" {
                    std::thread::spawn(|| {
                        std::thread::sleep(Duration::from_millis(200));
                        std::process::exit(4);
                    });
                }
                if mode == "deaf-after-ready" {
                    loop {
                        std::thread::sleep(Duration::from_secs(3600));
                    }
                }
            }
            Some("host.invoke") => {
                let inv = msg["params"].clone();
                let (tx, rx) = mpsc::channel();
                st.cancels
                    .lock()
                    .unwrap()
                    .insert(inv["invocationId"].as_str().unwrap_or("").to_owned(), tx);
                let (s, p) = (st.clone(), package.clone());
                std::thread::spawn(move || invoke(s, id, inv, rx, p));
            }
            Some("host.cancel") => {
                let inv = msg["params"]["invocationId"].clone();
                st.out.send(&result(
                    &id,
                    json!({"kind": "CancelAcknowledged", "invocationId": inv}),
                ));
                if let Some(tx) = inv
                    .as_str()
                    .and_then(|i| st.cancels.lock().unwrap().remove(i))
                {
                    let _ = tx.send(());
                }
            }
            Some("host.shutdown") => {
                // Records that the host asked, when a test watches for it (what a bounded
                // shutdown sends).
                let tmp = std::env::temp_dir();
                if tmp
                    .join(format!("devworks-fixture-{package}-shutdown-watch"))
                    .exists()
                {
                    let _ = std::fs::write(
                        tmp.join(format!("devworks-fixture-{package}-shutdown")),
                        msg["params"]["deadlineMs"].to_string(),
                    );
                }
                st.out
                    .send(&result(&id, json!({"kind": "ShutdownAcknowledged"})));
                return;
            }
            Some(_) => {}
        }
    }
}

// The helper speaks base64url and SHA-256 on its own rather than through `devworks_core`:
// linking any of core pulls its debug information into this debug binary, which on Linux grows
// it past the bundle limits the package tests install it under.

const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Unpadded base64url.
fn base64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(BASE64URL[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// base64url with or without padding; `None` on any other character.
fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.trim_end_matches('=').bytes() {
        let v = BASE64URL.iter().position(|&a| a == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Lowercase hex SHA-256 (FIPS 180-4).
fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = bytes.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((bytes.len() as u64) * 8).to_be_bytes());
    for block in message.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_local_encodings_match_core() {
        for input in [
            &b""[..],
            b"a",
            b"ab",
            b"abc",
            b"hello, world\n",
            &[0xff, 0x00, 0xfe, 0x7f][..],
        ] {
            assert_eq!(
                super::base64url(input),
                devworks_core::ports::http::base64url(input)
            );
            assert_eq!(
                super::base64url_decode(&super::base64url(input)).unwrap(),
                input
            );
            assert_eq!(
                super::sha256_hex(input),
                devworks_core::fingerprint::sha256_hex(input)
            );
        }
        let long = vec![b'x'; 1000];
        assert_eq!(
            super::sha256_hex(&long),
            devworks_core::fingerprint::sha256_hex(&long)
        );
        assert_eq!(super::base64url_decode("YWJj"), Some(b"abc".to_vec()));
        assert_eq!(super::base64url_decode("YQ=="), Some(b"a".to_vec()));
        assert_eq!(super::base64url_decode("Y+"), None);
    }
}
