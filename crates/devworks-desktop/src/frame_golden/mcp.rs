//! Sample frames of the Agent tools panel's MCP commands, by scope and action.
use super::{Silent, from};
use crate::mcp_panel::McpAction;
use crate::ops;
use devworks_core::operations::mcp;
use devworks_core::protocol::{Id, ProjectId};
use serde_json::json;

pub fn frames() -> Vec<(&'static str, Vec<u8>)> {
    let b = &Silent;
    let project: ProjectId = from(json!("project-a"));
    let server: Id = from(json!("server-a"));
    let tool: Id = from(json!("tool-a"));
    let mut out = Vec::new();
    for (scope_name, scope) in [("workspace", None), ("project", Some(&project))] {
        let send =
            |action: McpAction| ops::mcp_command(b, action.input(scope).expect("a command action"));
        let tools = vec![tool.clone(), from(json!("tool-b"))];
        let configure: mcp::ConfigureInput = from(json!({
            "scope": match scope {
                Some(p) => json!({"kind":"Project","projectId":p}),
                None => json!({"kind":"Workspace"}),
            },
            "name":"Docs",
            "transport":{"kind":"Http","endpoint":"https://mcp.example.com/v1","credentialRef":"secret-a"}}));
        for (name, action) in [
            ("connect", McpAction::Connect(server.clone(), 7)),
            ("disable", McpAction::Disable(server.clone(), 7)),
            ("remove", McpAction::Remove(server.clone(), 7, Vec::new())),
            ("discover", McpAction::Discover(server.clone(), 4)),
            ("select", McpAction::Select(server.clone(), 7, 4, tools)),
            (
                "classify",
                McpAction::Classify(server.clone(), 7, tool.clone(), None),
            ),
            (
                "classify_read_only",
                McpAction::Classify(server.clone(), 7, tool.clone(), Some(true)),
            ),
            (
                "configure_http",
                McpAction::Configure {
                    input: configure,
                    secret: None,
                },
            ),
        ] {
            out.push((
                &*Box::leak(format!("mcp_{name}_{scope_name}").into_boxed_str()),
                send(action),
            ));
        }
    }
    let configure = |extra: serde_json::Value| {
        let mut body = json!({"scope":{"kind":"Workspace"},"name":"Docs"});
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        ops::mcp_command(
            b,
            McpAction::Configure {
                input: from::<mcp::ConfigureInput>(body),
                secret: None,
            }
            .input(None)
            .expect("a command action"),
        )
    };
    out.push((
        "mcp_configure_update",
        configure(json!({"serverId":"server-a","expectedRevision":"7",
            "transport":{"kind":"Http","endpoint":"https://mcp.example.com/v1"}})),
    ));
    out.push((
        "mcp_configure_stdio",
        configure(json!({"name":"Local","transport":{"kind":"Stdio","process":
            {"executableRef":"exe-a","args":["--flag"],
            "workingDirectory":{"rootId":"root-a","relative":"work"},"environment":[],
            "profile":"Full","timeoutMs":"5000","outputLimitBytes":"1024"}}})),
    ));
    out
}
