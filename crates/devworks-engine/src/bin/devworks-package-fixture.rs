//! Test-only package helper that speaks the DevWorks package protocol (framed JSON-RPC over
//! stdin/stdout). The engine's integration tests install it inside signed fixture bundles; it
//! is not a product package and is never installed by the application itself.
//!
//! Behaviour comes from `fixture.json` in its working directory (the install directory, so it
//! is part of the verified, digest-bound package contents):
//!
//! * `mode`: `normal`, `hang-initialize` (never answers), `wrong-digest`, `wrong-inventory`,
//!   `exit-on-activate` (exits 3) or `exit-after-ready` (exits 4 shortly after activation);
//! * `inventory`: the contribution IDs it reports.
//!
//! Each invoked contribution's local name selects what the invocation does: `echo` (progress,
//! then completes with its input), `sleep` (completes after 300 ms), `wait` (until cancelled,
//! then `CancelledBeforeEffect`), `stubborn` (acknowledges cancellation but never finishes),
//! `state` (broker.state read then compare-and-set, results as progress), `event` (tries
//! broker.state compare-and-set and an HTTP broker.call, then broker.commit_event with the
//! commit read from `devworks-fixture-<package>-commit.json` in the temporary directory),
//! `config` (reports the configuration `host.activate` delivered as progress `config:<json>`,
//! or `config:none`) and `crash` (exits 5 mid-invocation).
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
            let mut params = ids;
            for (k, v) in commit.as_object().cloned().unwrap_or_default() {
                params[k] = v;
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
                let p = &msg["params"];
                *st.session.lock().unwrap() =
                    p["processSessionId"].as_str().unwrap_or("").to_owned();
                let digest = if mode == "wrong-digest" {
                    json!("f".repeat(64))
                } else {
                    p["expectedManifestDigest"].clone()
                };
                st.out.send(&result(
                    &id,
                    json!({"kind": "Initialized", "selected": p["offeredMax"],
                    "manifestDigest": digest, "contributionIds": inventory, "features": []}),
                ));
            }
            Some("host.activate") => {
                if mode == "exit-on-activate" {
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
                st.out
                    .send(&result(&id, json!({"kind": "ShutdownAcknowledged"})));
                return;
            }
            Some(_) => {}
        }
    }
}
