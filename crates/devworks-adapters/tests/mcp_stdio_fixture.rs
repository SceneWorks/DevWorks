//! A minimal stdio MCP server for the native client's process tests. Run as a test it does
//! nothing; launched with `DEVWORKS_MCP_STDIO_FIXTURE=1` it serves one `echo` tool until its
//! standard input closes.
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};

struct EchoServer;
impl rmcp::ServerHandler for EchoServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let mut tool = Tool::default();
        tool.name = "echo".into();
        let mut result = ListToolsResult::default();
        result.tools.push(tool);
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let text = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("text"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Ok(CallToolResponse::Complete(CallToolResult::success(vec![
            ContentBlock::text(format!("echo: {text}")),
        ])))
    }
}

fn main() {
    if std::env::var("DEVWORKS_MCP_STDIO_FIXTURE").as_deref() != Ok("1") {
        return;
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("fixture runtime")
        .block_on(async {
            let service = rmcp::serve_server(EchoServer, (tokio::io::stdin(), tokio::io::stdout()))
                .await
                .expect("fixture handshake");
            let _ = service.waiting().await;
        });
}
