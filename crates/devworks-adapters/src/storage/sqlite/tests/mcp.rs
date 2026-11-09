use super::Harness;
use crate::packages::JsonSchemaValidator;
use crate::storage::sqlite::SqliteStore;
use devworks_core::authority::HostAuthority;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::mcp::{
    McpEndpoint, McpRemote, McpRemoteFault, RemoteCatalog, RemoteTool,
};
use devworks_core::ports::storage::{
    ApplicationStore, AttemptBinding, ConfigScope, OperationRecord, ReadOptions, WriteOptions,
    read_snapshot, run_write,
};
use devworks_core::protocol::records::McpCapability;
use devworks_core::protocol::results::{DomainDecision, DomainQueryReply, OperationResult};
use devworks_core::protocol::support::BoundedText;
use devworks_core::protocol::{
    ActorId, CommandReply, Counter, Id, OperationId, OperationState, Revision,
};
use devworks_core::services::UseCases;
use devworks_core::services::mcp::{McpClientService, McpEffectOutcome};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct RecordedRemote(AtomicUsize);
impl McpRemote for RecordedRemote {
    fn connect(
        &self,
        _context: &InvocationContext,
        _endpoint: &McpEndpoint,
    ) -> Result<(), McpRemoteFault> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn discover(&self, _server_id: &Id) -> Result<RemoteCatalog, McpRemoteFault> {
        Ok(RemoteCatalog {
            tools: vec![RemoteTool {
                name: "lookup".into(),
                description: "Read a remote value".into(),
                input_schema: json!({"type":"object"}),
                output_schema: None,
                read_only_hint: true,
            }],
            resources: vec![],
            prompts: vec![],
        })
    }

    fn call_tool(
        &self,
        _server_id: &Id,
        _name: &str,
        _arguments: Value,
    ) -> Result<Value, McpRemoteFault> {
        Ok(json!({"content":[{"type":"text","text":"sample"}]}))
    }

    fn read_resource(&self, _server_id: &Id, _uri: &str) -> Result<Value, McpRemoteFault> {
        Ok(json!({"contents":[]}))
    }

    fn get_prompt(
        &self,
        _server_id: &Id,
        _name: &str,
        _arguments: Value,
    ) -> Result<Value, McpRemoteFault> {
        Ok(json!({"messages":[]}))
    }

    fn disconnect(&self, _server_id: &Id) -> Result<(), McpRemoteFault> {
        Ok(())
    }
}

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}

fn dispatched(store: &SqliteStore, id: &OperationId) -> OperationRecord {
    let id = id.clone();
    run_write(store, WriteOptions::default(), move |tx| {
        let current = tx.operations().operation(&id)?.expect("operation intent");
        let attempt =
            tx.operations()
                .begin_attempt(&id, current.revision, AttemptBinding::default())?;
        let preflighted = tx
            .operations()
            .operation(&id)?
            .expect("preflighted operation");
        tx.operations().transition(
            &id,
            preflighted.revision,
            Some(&attempt.fence()),
            OperationState::Dispatching,
            None,
        )
    })
    .unwrap()
}

fn context(store: &SqliteStore) -> InvocationContext {
    HostIngress::new(store).context(
        ActorId(id("mcp-user")),
        PrincipalKind::Human,
        id("mcp-invocation"),
        id("mcp-ui"),
        Counter::new(store.info().restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    )
}

fn command(
    h: &Harness,
    store: &SqliteStore,
    cases: &UseCases,
    key: &str,
    op: &str,
    mut body: Value,
) -> DomainDecision {
    body["kind"] = json!(op);
    let frame = json!({"mode":"Command","protocol":"1.0-draft.1","requestKey":key,"body":body});
    let authority = HostAuthority {
        store,
        clock: &*h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        authority: &authority,
        store,
        ids: &*h.ids,
    };
    match cases
        .command(&gateway, &context(store), op, frame.to_string().as_bytes())
        .expect("MCP command registered")
    {
        CommandReply::Recorded { receipt, .. } => receipt.decision,
        other => panic!("expected recorded command: {other:?}"),
    }
}

fn servers(
    h: &Harness,
    store: &SqliteStore,
    cases: &UseCases,
) -> Vec<devworks_core::protocol::records::McpServer> {
    let authority = HostAuthority {
        store,
        clock: &*h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        authority: &authority,
        store,
        ids: &*h.ids,
    };
    let frame = json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"mcp-list","body":{
        "kind":"mcp.servers","scope":{"kind":"Workspace"},"page":{"limit":10}
    }});
    match cases
        .query(
            &gateway,
            &context(store),
            "mcp.servers",
            frame.to_string().as_bytes(),
        )
        .expect("MCP query registered")
    {
        DomainQueryReply::Result {
            result: OperationResult::McpServers { value },
            ..
        } => value.items.0,
        other => panic!("expected MCP server page: {other:?}"),
    }
}

fn capabilities(
    h: &Harness,
    store: &SqliteStore,
    cases: &UseCases,
    server_id: &Id,
    catalog_revision: i64,
) -> DomainQueryReply {
    let authority = HostAuthority {
        store,
        clock: &*h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        authority: &authority,
        store,
        ids: &*h.ids,
    };
    let frame = json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"mcp-catalog","body":{
        "kind":"mcp.capabilities","serverId":server_id,
        "catalogRevision":catalog_revision.to_string(),"page":{"limit":10}
    }});
    cases
        .query(
            &gateway,
            &context(store),
            "mcp.capabilities",
            frame.to_string().as_bytes(),
        )
        .expect("MCP capabilities registered")
}

#[test]
fn configuration_is_durable_and_selection_rejects_unknown_tools() {
    let h = Harness::new();
    let store = Arc::new(h.store());
    run_write(&*store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")?;
        Ok(())
    })
    .unwrap();
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    let cases = UseCases::new().with_mcp(service.clone());
    let created = command(
        &h,
        &store,
        &cases,
        "mcp-create",
        "mcp.configure",
        json!({
            "scope":{"kind":"Workspace"},"name":"Local review server",
            "transport":{"kind":"Http","endpoint":"https://mcp.example.test/mcp"}
        }),
    );
    let DomainDecision::Applied {
        result: OperationResult::McpConfigure { value: server },
    } = created
    else {
        panic!("expected configured MCP server: {created:?}");
    };
    assert_eq!(server.revision.get(), 1);
    assert_eq!(servers(&h, &store, &cases).len(), 1);
    let selected = command(
        &h,
        &store,
        &cases,
        "mcp-select",
        "mcp.select_tools",
        json!({"serverId":server.id,"expectedRevision":"1","expectedCatalogRevision":"1","toolIds":["unknown-tool"]}),
    );
    assert!(matches!(selected, DomainDecision::Refused { .. }));
    let read = McpCapability {
        id: id("mcp-read"),
        server_id: server.id.clone(),
        kind: BoundedText("tool".into()),
        name: BoundedText("read".into()),
        description: BoundedText("Read a remote record".into()),
        input_schema: None,
        output_schema: None,
        read_only: true,
        revision: Revision::new(1).unwrap(),
    };
    let write = McpCapability {
        id: id("mcp-write"),
        name: BoundedText("write".into()),
        description: BoundedText("Write a remote record".into()),
        read_only: false,
        ..read.clone()
    };
    let changed_read = McpCapability {
        name: BoundedText("read_v2".into()),
        revision: Revision::new(2).unwrap(),
        ..read.clone()
    };
    let unchanged_write = write.clone();
    let service_for_write = service.clone();
    let server_id = server.id.clone();
    let published = run_write(&*store, WriteOptions::default(), move |tx| {
        service_for_write.publish_catalog(
            tx,
            &ConfigScope::Workspace,
            &server_id,
            &Revision::new(1).unwrap(),
            vec![read, write],
        )
    })
    .unwrap();
    assert_eq!(published.selected_tool_ids.0, vec![id("mcp-read")]);
    assert_eq!(published.catalog_revision.get(), 2);
    assert!(matches!(
        capabilities(&h, &store, &cases, &server.id, 2),
        DomainQueryReply::Result { result: OperationResult::McpCapabilities { value }, .. }
        if value.items.0.len() == 2
    ));
    assert!(matches!(
        capabilities(&h, &store, &cases, &server.id, 1),
        DomainQueryReply::Rejected { .. }
    ));
    let selected = command(
        &h,
        &store,
        &cases,
        "mcp-select-known",
        "mcp.select_tools",
        json!({"serverId":server.id,"expectedRevision":"2","expectedCatalogRevision":"2","toolIds":["mcp-read","mcp-write"]}),
    );
    assert!(matches!(
        selected,
        DomainDecision::Applied {
            result: OperationResult::McpSelectTools { .. }
        }
    ));
    let service_for_write = service.clone();
    let server_id = server.id.clone();
    let changed = run_write(&*store, WriteOptions::default(), move |tx| {
        service_for_write.publish_catalog(
            tx,
            &ConfigScope::Workspace,
            &server_id,
            &Revision::new(2).unwrap(),
            vec![changed_read, unchanged_write],
        )
    })
    .unwrap();
    assert_eq!(changed.selected_tool_ids.0, vec![id("mcp-write")]);
    drop(cases);
    drop(service);
    drop(store);
    let reopened = h.store();
    let cases = UseCases::new().with_mcp(Arc::new(McpClientService::new(h.ids.clone())));
    let saved = servers(&h, &reopened, &cases);
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].name.0, "Local review server");
    assert_eq!(saved[0].selected_tool_ids.0, vec![id("mcp-write")]);
}

#[test]
fn discovery_rejects_more_default_read_only_tools_than_selection_can_hold() {
    let h = Harness::new();
    let store = h.store();
    run_write(&store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")?;
        Ok(())
    })
    .unwrap();
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    let cases = UseCases::new().with_mcp(service.clone());
    let DomainDecision::Applied {
        result: OperationResult::McpConfigure { value: server },
    } = command(
        &h,
        &store,
        &cases,
        "mcp-bound-create",
        "mcp.configure",
        json!({
            "scope":{"kind":"Workspace"},"name":"Large server",
            "transport":{"kind":"Http","endpoint":"https://mcp.example.test/mcp"}
        }),
    )
    else {
        panic!("server configuration failed");
    };
    let catalog = (0..129)
        .map(|number| McpCapability {
            id: id(&format!("read-{number}")),
            server_id: server.id.clone(),
            kind: BoundedText("tool".into()),
            name: BoundedText(format!("read_{number}")),
            description: BoundedText("Read a record".into()),
            input_schema: None,
            output_schema: None,
            read_only: true,
            revision: Revision::new(1).unwrap(),
        })
        .collect();
    let service_for_write = service.clone();
    let server_id = server.id.clone();
    let result = run_write(&store, WriteOptions::default(), move |tx| {
        service_for_write.publish_catalog(
            tx,
            &ConfigScope::Workspace,
            &server_id,
            &Revision::new(1).unwrap(),
            catalog,
        )
    });
    assert!(matches!(
        result,
        Err(devworks_core::ports::storage::StorageError::InvalidRecord(
            _
        ))
    ));
    assert_eq!(servers(&h, &store, &cases)[0].catalog_revision.get(), 1);
}

#[test]
fn journaled_connect_rechecks_configuration_before_native_io() {
    let h = Harness::new();
    let store = Arc::new(h.store());
    run_write(&*store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")?;
        Ok(())
    })
    .unwrap();
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    let remote = Arc::new(RecordedRemote::default());
    service
        .install_remote(store.clone(), remote.clone(), Arc::new(JsonSchemaValidator))
        .unwrap();
    let cases = UseCases::new().with_mcp(service.clone());
    let DomainDecision::Applied {
        result: OperationResult::McpConfigure { value: server },
    } = command(
        &h,
        &store,
        &cases,
        "journal-server",
        "mcp.configure",
        json!({
            "scope":{"kind":"Workspace"},"name":"Journaled server",
            "transport":{"kind":"Http","endpoint":"https://mcp.example.test/mcp"}
        }),
    )
    else {
        panic!("server configuration failed");
    };
    let DomainDecision::Accepted { operation_id } = command(
        &h,
        &store,
        &cases,
        "journal-connect",
        "mcp.connect",
        json!({"serverId":server.id,"expectedRevision":"1"}),
    ) else {
        panic!("connect was not accepted");
    };
    assert_eq!(servers(&h, &store, &cases)[0].revision.get(), 2);
    assert_eq!(remote.0.load(Ordering::SeqCst), 0);
    let operation = dispatched(&store, &operation_id);
    assert!(matches!(
        service.execute(&operation),
        McpEffectOutcome::Completed(_)
    ));
    assert_eq!(remote.0.load(Ordering::SeqCst), 1);
    assert_eq!(servers(&h, &store, &cases)[0].revision.get(), 3);

    let DomainDecision::Accepted { operation_id } = command(
        &h,
        &store,
        &cases,
        "journal-reconnect",
        "mcp.connect",
        json!({"serverId":server.id,"expectedRevision":"3"}),
    ) else {
        panic!("reconnect was not accepted");
    };
    let changed = command(
        &h,
        &store,
        &cases,
        "journal-reconfigure",
        "mcp.configure",
        json!({
            "scope":{"kind":"Workspace"},"serverId":server.id,"expectedRevision":"4",
            "name":"Changed server","transport":{"kind":"Http","endpoint":"https://other.example.test/mcp"}
        }),
    );
    assert!(matches!(changed, DomainDecision::Applied { .. }));
    let operation = dispatched(&store, &operation_id);
    assert!(matches!(
        service.execute(&operation),
        McpEffectOutcome::Failed(_)
    ));
    assert_eq!(remote.0.load(Ordering::SeqCst), 1);
}

#[test]
fn remote_catalog_keeps_stable_schema_ids_and_invalidates_changed_selection() {
    let h = Harness::new();
    let store = Arc::new(h.store());
    run_write(&*store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")?;
        Ok(())
    })
    .unwrap();
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    let cases = UseCases::new().with_mcp(service.clone());
    let DomainDecision::Applied {
        result: OperationResult::McpConfigure { value: server },
    } = command(
        &h,
        &store,
        &cases,
        "catalog-server",
        "mcp.configure",
        json!({
            "scope":{"kind":"Workspace"},"name":"Catalog server",
            "transport":{"kind":"Http","endpoint":"https://mcp.example.test/mcp"}
        }),
    )
    else {
        panic!("server configuration failed");
    };
    let catalog = |schema| RemoteCatalog {
        tools: vec![RemoteTool {
            name: "lookup".into(),
            description: "Read a value".into(),
            input_schema: schema,
            output_schema: Some(json!({"type":"object"})),
            read_only_hint: true,
        }],
        resources: vec![],
        prompts: vec![],
    };
    let first = {
        let service = service.clone();
        let server_id = server.id.clone();
        run_write(&*store, WriteOptions::default(), move |tx| {
            service.publish_remote_catalog(
                tx,
                &ConfigScope::Workspace,
                &server_id,
                &Revision::new(1).unwrap(),
                catalog(json!({"type":"object","properties":{"key":{"type":"string"}}})),
            )
        })
        .unwrap()
    };
    assert_eq!(first.selected_tool_ids.0.len(), 1);
    let selected = read_snapshot(&*store, &ReadOptions::default(), |snapshot| {
        service.selected_tool_materials(&context(&store), snapshot)
    })
    .unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].schema["type"], "object");
    assert_eq!(
        selected[0].alias.capability_id.as_str().split('/').next(),
        Some("mcp")
    );
    let first_cap = match capabilities(&h, &store, &cases, &server.id, 2) {
        DomainQueryReply::Result {
            result: OperationResult::McpCapabilities { value },
            ..
        } => value.items.0.into_iter().next().unwrap(),
        other => panic!("unexpected catalog: {other:?}"),
    };
    assert!(first_cap.input_schema.is_some());
    let second = {
        let service = service.clone();
        let server_id = server.id.clone();
        run_write(&*store, WriteOptions::default(), move |tx| {
            service.publish_remote_catalog(
                tx,
                &ConfigScope::Workspace,
                &server_id,
                &Revision::new(2).unwrap(),
                catalog(json!({"properties":{"key":{"type":"string"}},"type":"object"})),
            )
        })
        .unwrap()
    };
    assert_eq!(second.selected_tool_ids.0, first.selected_tool_ids.0);
    let second_cap = match capabilities(&h, &store, &cases, &server.id, 3) {
        DomainQueryReply::Result {
            result: OperationResult::McpCapabilities { value },
            ..
        } => value.items.0.into_iter().next().unwrap(),
        other => panic!("unexpected catalog: {other:?}"),
    };
    assert_eq!(second_cap.revision, first_cap.revision);
    assert_eq!(
        second_cap.input_schema.unwrap().sha256,
        first_cap.input_schema.unwrap().sha256
    );
    let changed = {
        let service = service.clone();
        let server_id = server.id.clone();
        run_write(&*store, WriteOptions::default(), move |tx| {
            service.publish_remote_catalog(
                tx,
                &ConfigScope::Workspace,
                &server_id,
                &Revision::new(3).unwrap(),
                catalog(json!({"type":"object","properties":{"key":{"type":"number"}}})),
            )
        })
        .unwrap()
    };
    assert!(changed.selected_tool_ids.0.is_empty());
}

#[test]
fn journaled_discovery_publishes_selected_read_only_catalog() {
    let h = Harness::new();
    let store = Arc::new(h.store());
    run_write(&*store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")?;
        Ok(())
    })
    .unwrap();
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    let remote = Arc::new(RecordedRemote::default());
    service
        .install_remote(store.clone(), remote, Arc::new(JsonSchemaValidator))
        .unwrap();
    let cases = UseCases::new().with_mcp(service.clone());
    let DomainDecision::Applied {
        result: OperationResult::McpConfigure { value: server },
    } = command(
        &h,
        &store,
        &cases,
        "discovery-server",
        "mcp.configure",
        json!({
            "scope":{"kind":"Workspace"},"name":"Discovery server",
            "transport":{"kind":"Http","endpoint":"https://mcp.example.test/mcp"}
        }),
    )
    else {
        panic!("server configuration failed");
    };
    let DomainDecision::Accepted { operation_id } = command(
        &h,
        &store,
        &cases,
        "discovery-connect",
        "mcp.connect",
        json!({"serverId":server.id,"expectedRevision":"1"}),
    ) else {
        panic!("connect was not accepted");
    };
    let operation = dispatched(&store, &operation_id);
    assert!(matches!(
        service.execute(&operation),
        McpEffectOutcome::Completed(_)
    ));
    let DomainDecision::Accepted { operation_id } = command(
        &h,
        &store,
        &cases,
        "discovery-command",
        "mcp.discover",
        json!({"serverId":server.id,"expectedCatalogRevision":"1"}),
    ) else {
        panic!("discovery was not accepted");
    };
    let operation = dispatched(&store, &operation_id);
    assert!(matches!(
        service.execute(&operation),
        McpEffectOutcome::Completed(_)
    ));
    let published = servers(&h, &store, &cases).pop().unwrap();
    assert_eq!(published.catalog_revision.get(), 2);
    assert_eq!(published.selected_tool_ids.0.len(), 1);
    assert!(
        matches!(capabilities(&h, &store, &cases, &server.id, 2), DomainQueryReply::Result { result: OperationResult::McpCapabilities { value }, .. } if value.items.0.len() == 1)
    );
}
