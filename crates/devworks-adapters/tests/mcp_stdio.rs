//! A registered stdio MCP server is launched through the real process supervisor, discovered,
//! called and, after disconnect, reaped. This target has no test harness: run normally it is
//! the test, and it launches its own executable as the server with
//! `DEVWORKS_MCP_STDIO_SERVER=1`, so the server is always built with it and its standard output
//! carries only protocol bytes.
use devworks_adapters::fs::NativeFilesystem;
use devworks_adapters::mcp::NativeMcpClients;
use devworks_adapters::process::NativeProcessHost;
use devworks_adapters::storage::sqlite::{SqliteConfig, SqliteStore};
use devworks_core::authority::destructive::ProtectedRoots;
use devworks_core::context::{HostIngress, PrincipalKind};
use devworks_core::ports::mcp::{McpEndpoint, McpRemote};
use devworks_core::ports::secrets::SessionSecrets;
use devworks_core::ports::storage::{
    ApplicationStore, ConfigScope, EventFilter, ReadOptions, WriteOptions, read_snapshot, run_write,
};
use devworks_core::ports::{Clock, IdKind, IdSource};
use devworks_core::protocol::{ActorId, Counter, Id, Revision};
use devworks_core::services::process::ProcessSupervisor;
use devworks_core::services::process::executables::PathExecutables;
use devworks_core::services::process::tools::{
    RegisteredExecutable, RegisteredWorkingRoot, ToolBindings,
};
use devworks_core::services::resources::ResourceService;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SERVER_MODE: &str = "DEVWORKS_MCP_STDIO_SERVER";

/// Serves one `echo` tool.
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

fn serve() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("server runtime")
        .block_on(async {
            let service = rmcp::serve_server(EchoServer, (tokio::io::stdin(), tokio::io::stdout()))
                .await
                .expect("server handshake");
            let _ = service.waiting().await;
        });
}

struct FixedClock;
impl Clock for FixedClock {
    fn now_ms(&self) -> Counter {
        Counter::new(1_700_000_000_000).unwrap()
    }
}
struct Ids(AtomicU64);
impl IdSource for Ids {
    fn next_id(&self, kind: IdKind) -> Id {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        Id::new(format!("{kind:?}-{n}").to_lowercase()).unwrap()
    }
}

/// Removes the test's data and working roots.
struct Scratch(std::path::PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn reaped(store: &SqliteStore) -> usize {
    read_snapshot(store, &ReadOptions::default(), |snapshot| {
        Ok(snapshot
            .history()
            .page(&EventFilter::All, 0, 500)?
            .events
            .into_iter()
            .filter(|event| event.kind == "ProcessReaped")
            .count())
    })
    .unwrap()
}

fn a_registered_stdio_mcp_server_is_launched_called_and_reaped() {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("devworks-mcp-stdio-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&scratch.0);
    let (data, work_root) = (scratch.0.join("data"), scratch.0.join("root"));
    std::fs::create_dir_all(work_root.join("work")).unwrap();
    let clock = Arc::new(FixedClock);
    let ids = Arc::new(Ids(AtomicU64::new(0)));
    let store = Arc::new(
        SqliteStore::open(SqliteConfig::new(
            data,
            clock.clone(),
            ids.clone(),
            ActorId(Id::new("host").unwrap()),
        ))
        .expect("open store"),
    );
    run_write(&*store, WriteOptions::default(), |tx| {
        tx.actors().register(
            &ActorId(Id::new("user-a").unwrap()),
            PrincipalKind::Human,
            "user-a",
        )
    })
    .unwrap();
    let supervisor = Arc::new(ProcessSupervisor::new(
        store.clone(),
        Arc::new(NativeProcessHost),
        Arc::new(ResourceService::new(ids.clone(), clock.clone())),
    ));
    let bindings = ToolBindings::new(
        [RegisteredExecutable {
            id: Id::new("mcp-server").unwrap(),
            path: std::env::current_exe().unwrap(),
        }],
        [RegisteredWorkingRoot {
            id: Id::new("mcp-root").unwrap(),
            project: None,
            path: work_root.canonicalize().unwrap(),
        }],
    )
    .unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let clients = NativeMcpClients::new(
        supervisor,
        bindings,
        Arc::new(SessionSecrets::new()),
        "mcp-stdio-test".into(),
        Arc::new(NativeFilesystem::default()),
        ProtectedRoots::new(Vec::new(), Vec::new(), None),
        Vec::new(),
        runtime.handle().clone(),
    );
    let server = Id::new("stdio-server").unwrap();
    let endpoint = McpEndpoint {
        server_id: server.clone(),
        scope: ConfigScope::Workspace,
        transport: serde_json::from_value(json!({"kind":"Stdio","process":{
            "executableRef":"mcp-server","args":[],
            "workingDirectory":{"rootId":"mcp-root","relative":"work"},
            "environment":[{"name":SERVER_MODE,"value":"1"}],
            "profile":"Full","timeoutMs":"60000","outputLimitBytes":"1048576"}}))
        .unwrap(),
        sampling: false,
    };
    let human = HostIngress::new(&*store).context(
        ActorId(Id::new("user-a").unwrap()),
        PrincipalKind::Human,
        Id::new("invocation-1").unwrap(),
        Id::new("ui-main").unwrap(),
        Counter::new(store.info().restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    clients
        .connect(&human, &endpoint)
        .expect("the registered stdio server starts and completes the handshake");
    let catalog = clients.discover(&server).expect("discovery over stdio");
    assert_eq!(catalog.tools.len(), 1);
    assert_eq!(catalog.tools[0].name, "echo");
    let result = clients
        .call_tool(&server, "echo", json!({"text":"over stdio"}))
        .expect("tool call over stdio");
    assert_eq!(result["content"][0]["text"], "echo: over stdio");
    assert_eq!(reaped(&store), 0);
    clients.disconnect(&server).expect("disconnect");
    // Closing the session ends the owned child: it is reaped and none stays running.
    let deadline = Instant::now() + Duration::from_secs(20);
    while reaped(&store) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(reaped(&store), 1);
    drop(clients);
    drop(runtime);
}

/// `mcp.servers` asks for missing runtimes inside its own read. With one read connection, a
/// lookup that opened a second read would wait for the pool and then report an allowed,
/// installed runtime as missing; it must use the caller's snapshot instead.
fn missing_runtime_uses_the_callers_read_on_a_single_connection_pool() {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("devworks-mcp-runtime-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&scratch.0);
    let (data, bin, work_root) = (
        scratch.0.join("data"),
        scratch.0.join("bin"),
        scratch.0.join("root"),
    );
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(work_root.join("work")).unwrap();
    // `node` is in the default process executables; here it is this test's own executable.
    #[cfg(unix)]
    std::os::unix::fs::symlink(std::env::current_exe().unwrap(), bin.join("node")).unwrap();
    #[cfg(windows)]
    std::fs::copy(std::env::current_exe().unwrap(), bin.join("node.exe")).unwrap();
    let clock = Arc::new(FixedClock);
    let ids = Arc::new(Ids(AtomicU64::new(0)));
    let mut config = SqliteConfig::new(
        data,
        clock.clone(),
        ids.clone(),
        ActorId(Id::new("host").unwrap()),
    );
    config.read_connections = 1;
    let store = Arc::new(SqliteStore::open(config).expect("open store"));
    let supervisor = Arc::new(ProcessSupervisor::new(
        store.clone(),
        Arc::new(NativeProcessHost),
        Arc::new(ResourceService::new(ids.clone(), clock.clone())),
    ));
    let search = bin.clone().into_os_string();
    let executables = Arc::new(PathExecutables::from_settings_on(
        store.clone(),
        Arc::new(move || Some(search.clone())),
    ));
    let bindings = ToolBindings::new(
        std::iter::empty::<RegisteredExecutable>(),
        [RegisteredWorkingRoot {
            id: Id::new("mcp-root").unwrap(),
            project: None,
            path: work_root.canonicalize().unwrap(),
        }],
    )
    .unwrap()
    .with_path_executables(executables, None);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let clients = NativeMcpClients::new(
        supervisor,
        bindings,
        Arc::new(SessionSecrets::new()),
        "mcp-runtime-test".into(),
        Arc::new(NativeFilesystem::default()),
        ProtectedRoots::new(Vec::new(), Vec::new(), None),
        Vec::new(),
        runtime.handle().clone(),
    );
    let transport = |executable: &str| {
        serde_json::from_value(json!({"kind":"Stdio","process":{
            "executableRef":executable,"args":[],
            "workingDirectory":{"rootId":"mcp-root","relative":"work"},
            "environment":[],"profile":"Full","timeoutMs":"60000","outputLimitBytes":"1024"}}))
        .unwrap()
    };
    let started = Instant::now();
    let (allowed, unlisted) = read_snapshot(&*store, &ReadOptions::default(), |snapshot| {
        Ok((
            clients.missing_runtime(&transport("node"), snapshot.configuration()),
            clients.missing_runtime(&transport("mcp-server"), snapshot.configuration()),
        ))
    })
    .unwrap();
    assert_eq!(allowed, None, "an allowed, installed runtime is available");
    assert_eq!(unlisted.as_deref(), Some("mcp-server"));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the lookup waited for a second read connection: {:?}",
        started.elapsed()
    );
    drop(clients);
    drop(runtime);
}

fn main() {
    if std::env::var(SERVER_MODE).as_deref() == Ok("1") {
        serve();
        return;
    }
    // cargo-nextest lists a binary's tests before running them; answer in libtest's terse format.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--list") {
        if !args.iter().any(|a| a == "--ignored") {
            println!("a_registered_stdio_mcp_server_is_launched_called_and_reaped: test");
        }
        return;
    }
    a_registered_stdio_mcp_server_is_launched_called_and_reaped();
    println!("test a_registered_stdio_mcp_server_is_launched_called_and_reaped ... ok");
    missing_runtime_uses_the_callers_read_on_a_single_connection_pool();
    println!("test missing_runtime_uses_the_callers_read_on_a_single_connection_pool ... ok");
}
