//! Exercise the desktop-owned listeners and the CLI entry point using actual wire frames.
//! The protected credential store is injected for this test.

mod common;

use devworks_core::ports::secrets::Secret;
use devworks_core::protocol::results::{DomainDecision, DomainQueryReply, OperationResult};
use devworks_core::protocol::{CommandReply, Id};
use devworks_desktop::backend::Backend;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

const LEGACY: &str = "2025-11-25";
const MODERN: &str = "2026-07-28";
const WAIT: Duration = Duration::from_secs(10);

fn command(backend: &dyn Backend, operation: &str, key: &str, body: Value) -> OperationResult {
    let mut body = body;
    body["kind"] = json!(operation);
    let frame = json!({"protocol":"1.0-draft.1","mode":"Command","requestKey":key,"body":body});
    let reply = backend
        .command(operation, frame.to_string().as_bytes())
        .unwrap();
    match reply {
        CommandReply::Recorded { receipt, .. } => match receipt.decision {
            DomainDecision::Applied { result } => result,
            other => panic!("unexpected {operation} decision: {other:?}"),
        },
        other => panic!("unexpected {operation} reply: {other:?}"),
    }
}

fn status(backend: &dyn Backend) -> devworks_core::protocol::records::McpInboundStatus {
    let reply = backend.query("mcp_server.status", br#"{"protocol":"1.0-draft.1","mode":"Query","queryId":"listener-status","body":{"kind":"mcp_server.status"}}"#).unwrap();
    let DomainQueryReply::Result {
        result: OperationResult::McpServerStatus { value },
        ..
    } = reply
    else {
        panic!("incoming MCP status was unavailable");
    };
    value
}

fn request(id: u32, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

fn initial() -> Value {
    request(
        1,
        "initialize",
        json!({
            "protocolVersion": LEGACY, "clientInfo":{"name":"wire-probe","version":"1"}, "capabilities":{}
        }),
    )
}

fn call(version: &str, project: &Id) -> Value {
    let params = json!({
        "name": "devworks_projects_get",
        "arguments": {"projectId":project, "input":{"projectId":project}},
    });
    request(2, "tools/call", profile(params, version))
}

fn profile(mut params: Value, version: &str) -> Value {
    if version == MODERN {
        params["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion": MODERN,
            "io.modelcontextprotocol/clientInfo": {"name":"wire-probe","version":"1"},
            "io.modelcontextprotocol/clientCapabilities": {}
        });
    }
    params
}

fn http(
    port: u16,
    method: &str,
    token: Option<&Secret>,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (u16, Value) {
    let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_write_timeout(Some(WAIT)).unwrap();
    tcp.set_read_timeout(Some(WAIT)).unwrap();
    let body = body.map_or(String::new(), |v| v.to_string());
    write!(tcp, "{method} /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\n", body.len()).unwrap();
    if let Some(token) = token {
        write!(tcp, "Authorization: Bearer {}\r\n", token.expose()).unwrap();
    }
    for (name, value) in headers {
        write!(tcp, "{name}: {value}\r\n").unwrap();
    }
    write!(tcp, "\r\n{body}").unwrap();
    let mut response = String::new();
    tcp.read_to_string(&mut response).unwrap();
    let (header, body) = response.split_once("\r\n\r\n").unwrap();
    assert!(!header.to_ascii_lowercase().contains("mcp-session-id"));
    let code = header
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let body = serde_json::from_str(body).unwrap_or(Value::Null);
    (code, body)
}

#[test]
fn native_bindings_use_the_gateway_in_both_mcp_wire_profiles() {
    let _case = common::native_runtime_case();
    let root = common::Root::new("incoming-mcp");
    let engine = common::start(&root);
    let runtime = engine.host().runtime();
    let missing: Vec<_> = devworks_core::services::mcp_inbound::published_tools()
        .iter()
        .filter(|tool| !runtime.has_mcp_tool(tool) || !tool.operation.supports(tool.revision))
        .map(|tool| tool.operation.name)
        .collect();
    eprintln!("Reviewed MCP bindings missing a native owner in this build: {missing:?}");
    if std::env::var_os("DEVWORKS_REQUIRE_ALL_MCP").is_some() {
        assert!(missing.is_empty(), "{missing:?}");
    }
    let backend = engine.backend();
    let project = command(
        &*backend,
        "projects.create",
        "create",
        json!({
            "name":"MCP wire project","description":"","repositoryIds":[]
        }),
    );
    let OperationResult::ProjectsCreate { value: project } = project else {
        panic!("project")
    };
    let port = TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    command(
        &*backend,
        "mcp_server.configure",
        "enable",
        json!({
            "expectedRevision":"1","enabled":true,"bindings":["Stdio","LoopbackHttp"],"httpPort":port
        }),
    );
    let registered = command(
        &*backend,
        "mcp_server.set_client_access",
        "register",
        json!({
            "action":{"kind":"Register","name":"Integration wire probe"}
        }),
    );
    let OperationResult::McpServerSetClientAccess { value: registered } = registered else {
        panic!("registration")
    };
    let activated = command(
        &*backend,
        "mcp_server.set_client_access",
        "activate",
        json!({
            "action":{
                "kind":"Replace","clientId":registered.client_id,"expectedRevision":registered.revision,
                "name":registered.name,"enabled":true,"projectIds":[project.id],"repositoryIds":[],
            "worktreeIds":[],"readOperations":["projects.get","history.command_receipt"],
            "commandOperations":["knowledge.record"]
            }
        }),
    );
    let OperationResult::McpServerSetClientAccess { value: active } = activated else {
        panic!("activation")
    };
    let token = backend
        .provision_inbound_mcp_credential(&active.client_id, &active.revision)
        .unwrap();
    let until = Instant::now() + WAIT;
    loop {
        let state = status(&*backend);
        if state.endpoint.is_some()
            && state.protocol_versions.0.len() == 2
            && matches!(
                state.state,
                devworks_core::protocol::records::ItemState::Enabled
                    | devworks_core::protocol::records::ItemState::Partial
            )
        {
            assert!(state.endpoint.is_some());
            assert_eq!(state.protocol_versions.0.len(), 2);
            break;
        }
        assert!(Instant::now() < until, "{state:?}");
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(http(port, "GET", None, &[], None).0, 405);
    assert_eq!(http(port, "POST", None, &[], Some(initial())).0, 401);
    let legacy_headers = [("Mcp-Protocol-Version", LEGACY)];
    assert_eq!(
        http(
            port,
            "POST",
            Some(&token),
            &legacy_headers,
            Some(call(LEGACY, &project.id.0))
        )
        .0,
        400
    );
    let (code, reply) = http(port, "POST", Some(&token), &[], Some(initial()));
    assert_eq!(code, 200, "{reply}");
    assert_eq!(reply["result"]["protocolVersion"], LEGACY);
    assert_eq!(
        http(
            port,
            "POST",
            Some(&token),
            &legacy_headers,
            Some(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        )
        .0,
        202
    );
    let expected: std::collections::BTreeSet<String> =
        devworks_core::services::mcp_inbound::published_tools()
            .iter()
            .filter(|tool| {
                runtime.has_mcp_tool(tool)
                    && tool.has_target_authority()
                    && tool.operation.supports(tool.revision)
            })
            .map(|tool| tool.name.clone())
            .collect();
    let mut cursor = None;
    let mut discovered = std::collections::BTreeSet::new();
    loop {
        let params = cursor
            .take()
            .map_or_else(|| json!({}), |cursor| json!({"cursor":cursor}));
        let (_, reply) = http(
            port,
            "POST",
            Some(&token),
            &legacy_headers,
            Some(request(8, "tools/list", params)),
        );
        for tool in reply["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("{reply}"))
        {
            let name = tool["name"].as_str().unwrap();
            assert!(discovered.insert(name.to_owned()), "duplicate tool {name}");
        }
        assert!(discovered.len() <= expected.len());
        cursor = reply["result"]["nextCursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(discovered, expected);
    for version in [LEGACY, MODERN] {
        let mut headers = vec![("Mcp-Protocol-Version", version)];
        if version == MODERN {
            headers.extend([
                ("Mcp-Method", "tools/call"),
                ("Mcp-Name", "devworks_projects_get"),
            ]);
        }
        let (code, reply) = http(
            port,
            "POST",
            Some(&token),
            &headers,
            Some(call(version, &project.id.0)),
        );
        assert_eq!(code, 200, "{reply}");
        assert_eq!(
            reply["result"]["structuredContent"]["result"]["value"]["name"], "MCP wire project",
            "{reply}"
        );
    }
    let store = engine.host().runtime().store().info();
    let uri = format!(
        "devworks://context/{}/{}/{}/project/{}/current",
        store.store_id.0.as_str(),
        store.restore_epoch,
        project.id.0.as_str(),
        project.id.0.as_str()
    );
    let headers = [("Mcp-Protocol-Version", LEGACY)];
    let (_, resource) = http(
        port,
        "POST",
        Some(&token),
        &headers,
        Some(request(3, "resources/read", json!({"uri": uri}))),
    );
    let result: Value = serde_json::from_str(
        resource["result"]["contents"][0]["text"]
            .as_str()
            .expect("scoped resource result"),
    )
    .unwrap();
    assert_eq!(result["result"]["value"]["name"], "MCP wire project");
    assert_eq!(result["stamp"]["storeId"], store.store_id.0.as_str());
    let (_, prompt) = http(
        port,
        "POST",
        Some(&token),
        &headers,
        Some(request(
            4,
            "prompts/get",
            json!({"name": "devworks_project_context", "arguments": {"uri": uri}}),
        )),
    );
    assert!(
        prompt["result"]["messages"][0]["content"]["text"]
            .as_str()
            .is_some_and(|value| value.contains("MCP wire project")),
        "{prompt}"
    );
    assert_eq!(
        http(
            port,
            "POST",
            Some(&token),
            &[
                ("Mcp-Protocol-Version", MODERN),
                ("Mcp-Method", "tools/list")
            ],
            Some(call(MODERN, &project.id.0))
        )
        .0,
        400
    );

    for version in [LEGACY, MODERN] {
        stdio(&root.0, &token, version, &project.id.0);
    }

    let command_input = json!({
        "name": "devworks_knowledge_record",
        "arguments": {
            "projectId": project.id, "requestKey": "wire-knowledge",
            "input": {
                "scope": {"kind":"Project","projectId":project.id},
                "batch": {
                    "entities":[{"clientId":"subject","label":"Wire probe","entityType":"topic","aliases":[]}],
                    "claims":[{"subjectId":"subject","kind":"Fact","predicate":"notes",
                        "value":"A scoped native wire probe","confidence":0.9,"evidenceIds":[],"tags":[]}],
                    "evidence":[]
                }
            }
        }
    });
    let (_, first) = http(
        port,
        "POST",
        Some(&token),
        &legacy_headers,
        Some(request(5, "tools/call", command_input.clone())),
    );
    let first = &first["result"]["structuredContent"];
    assert_eq!(first["kind"], "Recorded", "{first}");
    let modern = [
        ("Mcp-Protocol-Version", MODERN),
        ("Mcp-Method", "tools/call"),
        ("Mcp-Name", "devworks_knowledge_record"),
    ];
    let (_, replay) = http(
        port,
        "POST",
        Some(&token),
        &modern,
        Some(request(6, "tools/call", profile(command_input, MODERN))),
    );
    assert_eq!(
        replay["result"]["structuredContent"]["receipt"]["commandId"],
        first["receipt"]["commandId"],
        "{replay}"
    );
    let until = Instant::now() + WAIT;
    let lookup = loop {
        let (_, reply) = http(
            port,
            "POST",
            Some(&token),
            &legacy_headers,
            Some(request(
                7,
                "tools/call",
                json!({
                    "name":"devworks_history_command_receipt",
                    "arguments":{"projectId":project.id,"input":{"query":{"requestKey":"wire-knowledge"}}}
                }),
            )),
        );
        if reply["result"]["structuredContent"]["fault"]["code"] != "ViewUnavailable" {
            break reply;
        }
        assert!(Instant::now() < until, "{reply}");
        std::thread::sleep(Duration::from_millis(30));
    };
    assert_eq!(
        lookup["result"]["structuredContent"]["result"]["value"]["commandId"],
        first["receipt"]["commandId"],
        "{lookup}"
    );

    #[cfg(unix)]
    if let Some(probe) = std::env::var_os("DEVWORKS_MCP_INTEROP_COMMAND") {
        let status = std::process::Command::new(probe)
            .env(
                "DEVWORKS_MCP_INTEROP_URL",
                format!("http://127.0.0.1:{port}/mcp"),
            )
            .env("DEVWORKS_MCP_INTEROP_PROJECT", project.id.0.as_str())
            .env(
                "DEVWORKS_MCP_WIRE_TEST_EXE",
                std::env::current_exe().unwrap(),
            )
            .env("DEVWORKS_MCP_WIRE_TEST_ROOT", &root.0)
            .env("DEVWORKS_MCP_TOKEN", token.expose())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "independent MCP client probe returned {status}"
        );
    }

    command(
        &*backend,
        "mcp_server.set_client_access",
        "revoke",
        json!({"action":{
            "kind":"Revoke","clientId":active.client_id,"expectedRevision":active.revision
        }}),
    );
    assert_eq!(
        http(port, "POST", Some(&token), &[], Some(initial())).0,
        401
    );
    command(
        &*backend,
        "mcp_server.configure",
        "disable",
        json!({"expectedRevision":"2","enabled":false,"bindings":[]}),
    );
    assert!(matches!(
        status(&*backend).state,
        devworks_core::protocol::records::ItemState::Disabled
    ));
    assert!(engine.shutdown().is_some());
}

#[test]
fn bridge_child_uses_the_test_desktop_image() {
    if let Some(root) = std::env::var_os("DEVWORKS_MCP_WIRE_TEST_ROOT") {
        // Separate the Rust test harness prefix from the wire frames.
        println!();
        std::io::stdout().flush().unwrap();
        devworks_desktop::run_mcp_bridge(std::path::Path::new(&root)).unwrap();
    }
}

#[test]
fn cli_bridge_leaves_a_missing_profile_untouched() {
    let root = common::Root::new("no-running-desktop");
    let absent = root.0.join("absent-profile");
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_devworks-desktop"))
        .arg("mcp")
        .arg("--data-dir")
        .arg(&absent)
        .env("DEVWORKS_MCP_TOKEN", "unregistered-test-client")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!status.status.success());
    assert!(status.stdout.is_empty());
    assert!(!absent.exists());
}

fn stdio(root: &std::path::Path, token: &Secret, version: &str, project: &Id) {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;

    struct Bridge(Child);
    impl Drop for Bridge {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut bridge = Bridge(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bridge_child_uses_the_test_desktop_image",
                "--nocapture",
            ])
            .env("DEVWORKS_MCP_WIRE_TEST_ROOT", root)
            .env("DEVWORKS_MCP_TOKEN", token.expose())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdin = bridge.0.stdin.take().unwrap();
    let stdout = bridge.0.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.starts_with('{') && sender.send(line).is_err() {
                break;
            }
        }
    });
    let mut send = |v: Value| writeln!(stdin, "{v}").unwrap();
    let read = || serde_json::from_str::<Value>(&receiver.recv_timeout(WAIT).unwrap()).unwrap();
    if version == LEGACY {
        send(initial());
        let initialize = read();
        assert_eq!(
            initialize["result"]["protocolVersion"], LEGACY,
            "{initialize}"
        );
        send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    } else {
        send(request(1, "server/discover", profile(json!({}), MODERN)));
        let discover = read();
        assert!(discover["result"].is_object(), "{discover}");
    }
    send(call(version, project));
    let reply = read();
    assert_eq!(
        reply["result"]["structuredContent"]["result"]["value"]["name"], "MCP wire project",
        "{reply}"
    );
    drop(stdin);
    let until = Instant::now() + WAIT;
    loop {
        if let Some(exit) = bridge.0.try_wait().unwrap() {
            assert!(exit.success(), "{exit}");
            break;
        }
        assert!(
            Instant::now() < until,
            "stdio bridge did not close after EOF"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
