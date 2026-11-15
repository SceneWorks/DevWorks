//! Brokered HTTP through named host connections: a package granted `network:<endpoint>` reaches
//! it only through a configured connection, the connection's credential is added by the host
//! and never reaches the package, an ungranted endpoint and a refused address are refused, and
//! every call is journaled against the invocation that made it.
use super::broker::{read_artifact, run_calls};
use super::*;
use devworks_core::ports::secrets::{Secret, SecretRef, SecretResolver, SecretUnavailable};
use devworks_core::ports::web::{
    HopRequest, HopResponse, HopTransport, NetFault, Resolver as NameResolver,
};
use devworks_core::protocol::OperationState;
use devworks_core::settings::catalog::PACKAGE_CONNECTIONS_KEY;
use devworks_engine::package_host::http::{HTTP_CAPABILITY, NetworkPorts};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};

const SECRET: &str = "s3cret-connection-token-0f9e8d7c";
const SECRET_REF: &str = "dwsecret-0123456789abcdef0123456789abcdef";

/// Resolves the test hosts: the API to a public address, the metadata host to link-local.
struct Names;
impl NameResolver for Names {
    fn resolve(
        &self,
        host: &str,
        _: u16,
        _: Instant,
        _: &[devworks_core::ports::storage::Cancellation],
    ) -> Result<Vec<IpAddr>, NetFault> {
        match host {
            "api.example.test" => Ok(vec!["93.184.216.34".parse().unwrap()]),
            "metadata.example.test" => Ok(vec!["169.254.169.254".parse().unwrap()]),
            _ => Err(NetFault::NotSent {
                reason: "unknown host".into(),
                timed_out: false,
            }),
        }
    }
}

/// One request the server saw.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    addr: SocketAddr,
    host: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Answers every request 200, echoing the authorization it received (as a careless server
/// might), and records what it was sent. A request for `/big` is answered with [`Server::big`];
/// one for `/stall` sends one chunk and then waits until the call is cancelled, after which every
/// read is `Interrupted` (as the native transport answers), counting the reads.
#[derive(Default)]
struct Server {
    seen: Mutex<Vec<Seen>>,
    big: Mutex<Vec<u8>>,
    reads: Arc<AtomicUsize>,
}

/// The body of a stalled answer.
struct Stalled {
    first: Option<Vec<u8>>,
    cancel: Vec<devworks_core::ports::storage::Cancellation>,
    reads: Arc<AtomicUsize>,
}
impl std::io::Read for Stalled {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if let Some(first) = self.first.take() {
            buf[..first.len()].copy_from_slice(&first);
            return Ok(first.len());
        }
        loop {
            if self.cancel.iter().any(|c| c.is_cancelled()) {
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl HopTransport for Server {
    fn get(&self, request: &HopRequest<'_>) -> Result<HopResponse, NetFault> {
        self.send(request, "GET", &[])
    }
    fn send(
        &self,
        request: &HopRequest<'_>,
        method: &str,
        body: &[u8],
    ) -> Result<HopResponse, NetFault> {
        let headers: Vec<(String, String)> = request
            .headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let auth = headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        self.seen.lock().unwrap().push(Seen {
            method: method.into(),
            addr: request.addr,
            host: request.host_header.into(),
            target: request.target.into(),
            headers,
            body: body.to_vec(),
        });
        let answer: Box<dyn std::io::Read + Send> = match request.target {
            "/v1/big" => Box::new(std::io::Cursor::new(self.big.lock().unwrap().clone())),
            "/v1/stall" => Box::new(Stalled {
                first: Some(b"{\"partial\":".to_vec()),
                cancel: request.cancel.clone(),
                reads: self.reads.clone(),
            }),
            _ => Box::new(std::io::Cursor::new(
                json!({"ok": true, "youSent": auth})
                    .to_string()
                    .into_bytes(),
            )),
        };
        Ok(HopResponse {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("Set-Cookie".into(), "session=1".into()),
                ("X-Echo".into(), auth.clone()),
            ],
            body: answer,
        })
    }
}

struct Secrets;
impl SecretResolver for Secrets {
    fn resolve(&self, reference: &SecretRef) -> Result<Secret, SecretUnavailable> {
        (reference.as_str() == SECRET_REF)
            .then(|| Secret::new(SECRET))
            .ok_or(SecretUnavailable::NotFound)
    }
}

fn set_connections(store: &dyn ApplicationStore, connections: Value) {
    write(store, move |tx| {
        let current = model::current_layer(tx.configuration(), &ConfigScope::Workspace)?;
        let schema =
            serde_json::to_value(descriptor(PACKAGE_CONNECTIONS_KEY).unwrap().schema_ref())
                .unwrap();
        let a = serde_json::from_value(json!({"key": PACKAGE_CONNECTIONS_KEY,
            "value": {"schema": schema, "value": connections}}))
        .unwrap();
        let plan = model::plan(&current, &[a]);
        let body = model::body(
            &plan,
            &ActorId(id("user-a")),
            tx.now_ms(),
            json!({"kind": "test"}),
        );
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            model::NAMESPACE,
            current.revision,
            model::BODY_SCHEMA_VERSION,
            body,
        )?;
        Ok(())
    })
    .unwrap();
}

fn http(connection: &str, endpoint: &str, method: &str, path: &str, body: Option<Value>) -> Value {
    let mut call = json!({"kind": "Http", "connectionHandle": connection, "endpointId": endpoint,
        "httpMethod": method, "relativePath": path});
    if let Some(body) = body {
        call["body"] = body;
    }
    call
}

#[test]
fn a_granted_package_calls_its_endpoint_through_a_named_connection_without_seeing_its_credential() {
    let _s = serial();
    let a = App::new();
    let server = Arc::new(Server::default());
    a.app().packages().unwrap().attach_network(NetworkPorts {
        resolver: Arc::new(Names),
        transport: server.clone(),
        secrets: Arc::new(Secrets),
    });
    set_connections(
        &*a.store,
        json!([
            {"name": "main", "endpointId": "api", "baseUrl": "https://api.example.test/v1",
                "credential": {"secretRef": SECRET_REF}},
            {"name": "meta", "endpointId": "api", "baseUrl": "https://metadata.example.test"},
            {"name": "other", "endpointId": "other", "baseUrl": "https://api.example.test"},
        ]),
    );
    // Per process: concurrent copies of this binary never share the fixture's files.
    let name = format!("net-pack-{}", std::process::id());
    let mut spec = Spec::new(&name, "1.0.0");
    spec.contributions = vec!["call"];
    spec.permissions = vec![json!({"kind": "Network", "endpointId": "api"})];
    package(a.install(&a.user, &spec));
    a.ready(&name);
    let input = a.save(&a.user, br#"{"item":"widget"}"#);
    let script = json!({"dumpEnv": true, "calls": [
        {"actionKey": "post", "call": http("main", "api", "Post", "/items%20all?page=1", Some(json!("$input")))},
        // The manifest grants no network access to this endpoint.
        {"actionKey": "ungranted", "call": http("other", "other", "Get", "/items", None)},
        // A granted endpoint whose connection resolves to a refused (metadata) address.
        {"actionKey": "metadata", "call": http("meta", "api", "Get", "/latest", None)},
        // Paths that would leave the connection's base path, directly or once a server decodes
        // and normalises them.
        {"actionKey": "escape", "call": http("main", "api", "Get", "/%2e%2e/admin", None)},
        {"actionKey": "slash", "call": http("main", "api", "Get", "/..%2fadmin", None)},
        {"actionKey": "backslash", "call": http("main", "api", "Get", "/..%5Cadmin", None)},
        {"actionKey": "nested", "call": http("main", "api", "Get", "/%252e%252e/admin", None)},
        {"actionKey": "nested-slash", "call": http("main", "api", "Get", "/..%252Fadmin", None)},
        // Control characters the URL parser drops, which would join a split encoding.
        {"actionKey": "tab", "call": http("main", "api", "Get", "/..%2\tfadmin", None)},
        {"actionKey": "newline", "call": http("main", "api", "Get", "/..%2\nfadmin", None)},
        {"actionKey": "tab-early", "call": http("main", "api", "Get", "/..%\t2fadmin", None)},
        // Encodings of other characters that decode, round by round, to a separator or dot.
        {"actionKey": "encoded-f", "call": http("main", "api", "Get", "/..%252%66admin", None)},
        {"actionKey": "encoded-e", "call": http("main", "api", "Get", "/%2%65%2e/admin", None)},
        // No connection of this name serves the endpoint.
        {"actionKey": "unknown", "call": http("missing", "api", "Get", "/items", None)},
    ]});
    let (progress, outcome) = run_calls(&a, &name, script, &input, Some(a.agent.clone()), vec![]);
    assert_eq!(
        progress,
        vec![
            "call:post:BrokerCompleted",
            "call:ungranted:PermissionDenied",
            "call:metadata:PermissionDenied",
            "call:escape:PermissionDenied",
            "call:slash:PermissionDenied",
            "call:backslash:PermissionDenied",
            "call:nested:PermissionDenied",
            "call:nested-slash:PermissionDenied",
            "call:tab:PermissionDenied",
            // The wire protocol already refuses a line break in a relative path.
            "call:newline:ProtocolViolation",
            "call:tab-early:PermissionDenied",
            "call:encoded-f:PermissionDenied",
            "call:encoded-e:PermissionDenied",
            "call:unknown:PermissionDenied",
        ]
    );
    // Exactly one request left the host: the granted one, to the checked public address, with
    // the credential the host added and the invocation's body.
    let seen = server.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let sent = &seen[0];
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.addr, "93.184.216.34:443".parse().unwrap());
    assert_eq!(sent.host, "api.example.test");
    assert_eq!(sent.target, "/v1/items%20all?page=1");
    assert!(
        sent.headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == SECRET),
        "{:?}",
        sent.headers.iter().map(|(k, _)| k).collect::<Vec<_>>()
    );
    assert_eq!(sent.body, br#"{"item":"widget"}"#);
    // The package got the answer, without cookies and with the echoed credential replaced.
    let InvocationOutcome::Completed { artifacts } = outcome else {
        panic!("{outcome:?}");
    };
    let answer = read_artifact(&a, &a.agent, &artifacts[0]);
    assert_eq!(answer["status"], 200, "{answer}");
    let body =
        devworks_core::ports::http::base64url_decode(answer["bodyBase64Url"].as_str().unwrap())
            .unwrap();
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("[redacted]"), "{body}");
    let headers = answer["headers"].to_string();
    assert!(
        !headers.to_ascii_lowercase().contains("set-cookie"),
        "{headers}"
    );
    // The credential appears nowhere the package can see: its answer, its input, its progress
    // or its process environment.
    let env_path = std::env::temp_dir().join(format!("devworks-fixture-{name}-env"));
    let env = std::fs::read_to_string(&env_path).unwrap();
    let _ = std::fs::remove_file(&env_path);
    assert!(!env.is_empty());
    let input_bytes = read_artifact(&a, &a.user, &serde_json::from_value(input).unwrap());
    for (what, text) in [
        ("answer", answer.to_string()),
        ("input", input_bytes.to_string()),
        ("progress", format!("{progress:?}")),
        ("environment", env),
    ] {
        assert!(
            !text.contains(SECRET),
            "the credential reached the package's {what}"
        );
    }
    // Each call that passed its grant is journaled against the invocation that made it: its
    // durable operation and invocation, the connection, endpoint, method and outcome.
    let calls = read(&*a.store, |s| {
        s.operations().operations_for_capability(
            HTTP_CAPABILITY,
            &[OperationState::ObservedComplete, OperationState::Failed],
            None,
            10,
        )
    });
    let done: Vec<_> = calls
        .iter()
        .filter(|c| c.state == OperationState::ObservedComplete)
        .collect();
    assert_eq!(done.len(), 1, "{calls:?}");
    let call = done[0];
    assert_eq!(call.payload["packageId"], json!(name));
    assert_eq!(
        call.payload["contributionId"],
        json!(format!("{name}/call"))
    );
    assert_eq!(call.payload["connection"], "main");
    assert_eq!(call.payload["endpointId"], "api");
    assert_eq!(call.payload["method"], "POST");
    assert_eq!(call.payload["path"], "/v1/items%20all");
    assert!(
        call.payload["invocationOperationId"]
            .as_str()
            .is_some_and(|o| o.starts_with("op-"))
    );
    assert!(call.payload["invocationId"].as_str().is_some());
    let recorded = format!("{:?}", call.attempts);
    assert!(recorded.contains("\"status\": Number(200)"), "{recorded}");
    assert!(!format!("{calls:?}").contains(SECRET));
    // The refused address was journaled as a failed call naming its class; nothing was sent.
    let refused: Vec<_> = calls
        .iter()
        .filter(|c| c.state == OperationState::Failed)
        .collect();
    assert_eq!(refused.len(), 1, "{calls:?}");
    assert_eq!(refused[0].payload["connection"], "meta");
    assert!(
        format!("{:?}", refused[0].attempts).contains("\"refused\": String(\"Metadata\")"),
        "{refused:?}"
    );
    // The call started event is attributed to the package's own actor.
    let started = read(&*a.store, |s| {
        s.history().page(
            &EventFilter::Aggregate {
                kind: "package".into(),
                id: name.clone(),
            },
            0,
            500,
        )
    })
    .events
    .into_iter()
    .filter(|e| e.kind == "PackageBrokerCallStarted")
    .count();
    assert_eq!(started, 2);
}

#[test]
fn package_connections_accept_only_https_bases_and_secret_store_credentials() {
    let d = descriptor(PACKAGE_CONNECTIONS_KEY).unwrap();
    let check = |v: Value| devworks_core::settings::validate::check_value(d, &v).is_empty();
    assert!(check(json!([{"name": "main", "endpointId": "api",
        "baseUrl": "https://api.example.test/v1", "credential": {"secretRef": SECRET_REF}}])));
    for bad in [
        json!([{"name": "main", "endpointId": "api", "baseUrl": "http://api.example.test"}]),
        json!([{"name": "main", "endpointId": "api", "baseUrl": "https://api.example.test/?q=1"}]),
        json!([{"name": "main", "endpointId": "api", "baseUrl": "https://u:p@api.example.test"}]),
        json!([{"name": "main", "endpointId": "api", "baseUrl": "https://api.example.test",
            "credential": "raw-token"}]),
        json!([{"name": "main", "endpointId": "api", "baseUrl": "https://api.example.test",
            "credential": {"secretRef": SECRET_REF}, "credentialHeader": "Host"}]),
        json!([{"name": "a", "endpointId": "api", "baseUrl": "https://api.example.test"},
            {"name": "a", "endpointId": "api", "baseUrl": "https://api.example.test"}]),
    ] {
        assert!(!check(bad.clone()), "{bad}");
    }
}

/// A booted app with the test network attached, the `main` connection configured and a package
/// granted `network:api`; its name.
fn connected(a: &App, server: &Arc<Server>) -> String {
    a.app().packages().unwrap().attach_network(NetworkPorts {
        resolver: Arc::new(Names),
        transport: server.clone(),
        secrets: Arc::new(Secrets),
    });
    set_connections(
        &*a.store,
        json!([{"name": "main", "endpointId": "api", "baseUrl": "https://api.example.test/v1",
            "credential": {"secretRef": SECRET_REF}}]),
    );
    let name = format!("net-pack-{}", std::process::id());
    let mut spec = Spec::new(&name, "1.0.0");
    spec.contributions = vec!["call"];
    spec.permissions = vec![json!({"kind": "Network", "endpointId": "api"})];
    package(a.install(&a.user, &spec));
    a.ready(&name);
    name
}

/// Starts `name`'s `call` tool with `script` under the invocation operation `operation`.
fn start(
    a: &App,
    name: &str,
    script: Value,
    input: &Value,
    operation: &str,
) -> devworks_engine::package_host::InvocationHandle {
    let path = std::env::temp_dir().join(format!("devworks-fixture-{name}-call.json"));
    std::fs::write(&path, script.to_string()).unwrap();
    a.app()
        .packages()
        .unwrap()
        .invoke(InvokeRequest {
            package_id: name.into(),
            contribution_id: format!("{name}/call"),
            scope: InvocationScope::Workspace,
            input: serde_json::from_value(input.clone()).unwrap(),
            operation_id: id(operation),
            deadline: Duration::from_secs(30),
            allowed_operations: vec![],
            kind: InvocationKind::Ordinary,
            principal: Some(a.agent.clone()),
            expected_digest: None,
        })
        .unwrap()
}

fn answer_body(a: &App, artifact: &wire::ArtifactRef) -> (Value, Vec<u8>) {
    // The envelope of a 1 MiB body is larger than the shared helper reads.
    let reference = serde_json::from_value(serde_json::to_value(artifact).unwrap()).unwrap();
    let (bytes, _) = a
        .app()
        .artifacts()
        .read_exact(&*a.store, &a.agent, &reference, 4 << 20)
        .unwrap_or_else(|e| panic!("{e:?}"));
    let answer: Value = serde_json::from_slice(&bytes).unwrap();
    let body =
        devworks_core::ports::http::base64url_decode(answer["bodyBase64Url"].as_str().unwrap())
            .unwrap();
    (answer, body)
}

#[test]
fn a_credential_the_answer_bound_cuts_through_is_still_scrubbed() {
    let _s = serial();
    let a = App::new();
    let server = Arc::new(Server::default());
    let name = connected(&a, &server);
    let max = devworks_engine::package_host::http::MAX_BODY;
    // The credential starts 10 bytes before the kept bound and runs past it.
    let mut big = vec![b'a'; max - 10];
    big.extend_from_slice(SECRET.as_bytes());
    big.extend_from_slice(&[b'b'; 100]);
    *server.big.lock().unwrap() = big;
    let input = a.save(&a.user, b"{}");
    let script =
        json!({"calls": [{"actionKey": "big", "call": http("main", "api", "Get", "/big", None)}]});
    let (progress, outcome) = run_calls(&a, &name, script, &input, Some(a.agent.clone()), vec![]);
    assert_eq!(progress, vec!["call:big:BrokerCompleted"]);
    let InvocationOutcome::Completed { artifacts } = outcome else {
        panic!("{outcome:?}");
    };
    let (answer, body) = answer_body(&a, &artifacts[0]);
    assert_eq!(answer["truncated"], true, "{answer}");
    assert!(body.len() <= max);
    let text = String::from_utf8(body).unwrap();
    // No part of the credential survives the cut: not its first 10 bytes, not any shorter start.
    assert!(
        !text.contains(&SECRET[..10]),
        "the cut kept the credential's start"
    );
    assert!(
        (2..SECRET.len()).all(|n| !text.ends_with(&SECRET[..n])),
        "the answer ends with a credential prefix"
    );
}

#[test]
fn a_retried_call_returns_its_recorded_answer_after_a_restart() {
    let _s = serial();
    let mut a = App::new();
    let server = Arc::new(Server::default());
    let name = connected(&a, &server);
    let input = a.save(&a.user, br#"{"item":"widget"}"#);
    let script = json!({"calls": [{"actionKey": "post", "call": http("main", "api", "Post", "/items", Some(json!("$input")))}]});
    let first = start(&a, &name, script.clone(), &input, "op-retried");
    let InvocationOutcome::Completed { artifacts: before } =
        finished(first.wait(Duration::from_secs(20)))
    else {
        panic!("the first invocation did not complete");
    };
    // The answer and the call's outcome were recorded together, with the status and byte counts.
    let call = read(&*a.store, |s| {
        s.operations().operations_for_capability(
            HTTP_CAPABILITY,
            &[OperationState::ObservedComplete],
            None,
            10,
        )
    })
    .remove(0);
    let recorded = format!("{:?}", call.attempts);
    for field in [
        "\"result\"",
        "\"status\": Number(200)",
        "\"requestBytes\": Number(17)",
        "\"responseBytes\"",
    ] {
        assert!(recorded.contains(field), "{field}: {recorded}");
    }
    // The host restarts; the invocation is retried under the same operation.
    a.shutdown();
    a.boot();
    a.app().packages().unwrap().attach_network(NetworkPorts {
        resolver: Arc::new(Names),
        transport: server.clone(),
        secrets: Arc::new(Secrets),
    });
    a.ready(&name);
    let again = start(&a, &name, script, &input, "op-retried");
    let outcome = finished(again.wait(Duration::from_secs(20)));
    assert_eq!(again.progress().0, vec!["call:post:BrokerCompleted"]);
    let InvocationOutcome::Completed { artifacts: after } = outcome else {
        panic!("{outcome:?}");
    };
    // The recorded answer, never a second request.
    assert_eq!(
        serde_json::to_value(&after[0]).unwrap(),
        serde_json::to_value(&before[0]).unwrap()
    );
    assert_eq!(server.seen.lock().unwrap().len(), 1);
}

#[test]
fn cancelling_a_call_mid_answer_ends_it_promptly_without_spinning() {
    let _s = serial();
    let a = App::new();
    let server = Arc::new(Server::default());
    let name = connected(&a, &server);
    let input = a.save(&a.user, b"{}");
    let script = json!({"calls": [{"actionKey": "stall", "call": http("main", "api", "Get", "/stall", None)}]});
    let h = start(&a, &name, script, &input, "op-stalled");
    // The answer has started arriving and its body now waits.
    let until = Instant::now() + PATIENCE;
    while server.reads.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < until, "the answer never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    let cancelled = Instant::now();
    let _ = h.cancel();
    // The call ends as cancelled (its request was sent, so its outcome is unknown) promptly.
    let ended = loop {
        let calls = read(&*a.store, |s| {
            s.operations().operations_for_capability(
                HTTP_CAPABILITY,
                &[OperationState::OutcomeUnknown],
                None,
                10,
            )
        });
        if !calls.is_empty() {
            break calls;
        }
        assert!(
            cancelled.elapsed() < Duration::from_secs(10),
            "the call never ended"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        format!("{:?}", ended[0].attempts).contains("cancelled"),
        "{ended:?}"
    );
    // Nothing keeps reading the cancelled answer.
    let settled = server.reads.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(500));
    let later = server.reads.load(Ordering::SeqCst);
    assert!(
        later <= settled + 1,
        "the body was read {} more times after cancellation",
        later - settled
    );
}
