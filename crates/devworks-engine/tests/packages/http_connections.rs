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
/// might), and records what it was sent.
#[derive(Default)]
struct Server(Mutex<Vec<Seen>>);
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
        self.0.lock().unwrap().push(Seen {
            method: method.into(),
            addr: request.addr,
            host: request.host_header.into(),
            target: request.target.into(),
            headers,
            body: body.to_vec(),
        });
        Ok(HopResponse {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("Set-Cookie".into(), "session=1".into()),
                ("X-Echo".into(), auth.clone()),
            ],
            body: Box::new(std::io::Cursor::new(
                json!({"ok": true, "youSent": auth})
                    .to_string()
                    .into_bytes(),
            )),
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
        {"actionKey": "post", "call": http("main", "api", "Post", "/items?page=1", Some(json!("$input")))},
        // The manifest grants no network access to this endpoint.
        {"actionKey": "ungranted", "call": http("other", "other", "Get", "/items", None)},
        // A granted endpoint whose connection resolves to a refused (metadata) address.
        {"actionKey": "metadata", "call": http("meta", "api", "Get", "/latest", None)},
        // A path that would leave the connection's base path.
        {"actionKey": "escape", "call": http("main", "api", "Get", "/%2e%2e/admin", None)},
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
            "call:unknown:PermissionDenied",
        ]
    );
    // Exactly one request left the host: the granted one, to the checked public address, with
    // the credential the host added and the invocation's body.
    let seen = server.0.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let sent = &seen[0];
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.addr, "93.184.216.34:443".parse().unwrap());
    assert_eq!(sent.host, "api.example.test");
    assert_eq!(sent.target, "/v1/items?page=1");
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
    assert_eq!(call.payload["path"], "/v1/items");
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
