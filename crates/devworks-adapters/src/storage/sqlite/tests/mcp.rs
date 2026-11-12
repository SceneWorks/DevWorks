use super::Harness;
use crate::fs::NativeFilesystem;
use crate::fs::artifacts::NativeArtifactFiles;
use crate::fs::sources::HostArtifactSources;
use crate::packages::JsonSchemaValidator;
use crate::storage::sqlite::SqliteStore;
use devworks_core::authority::HostAuthority;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::mcp::{
    McpEndpoint, McpRemote, McpRemoteFault, RemoteCatalog, RemotePrompt, RemotePromptArgument,
    RemoteResource, RemoteTool,
};
use devworks_core::ports::storage::{
    ApplicationStore, AttemptBinding, ConfigScope, OperationRecord, ReadOptions, StoredProject,
    WriteOptions, read_snapshot, run_write,
};
use devworks_core::protocol::records::McpCapability;
use devworks_core::protocol::results::{DomainDecision, DomainQueryReply, OperationResult};
use devworks_core::protocol::support::BoundedText;
use devworks_core::protocol::{
    ActorId, CommandReply, Counter, Id, OperationId, OperationState, ProjectId, Revision,
};
use devworks_core::services::UseCases;
use devworks_core::services::artifacts::ArtifactService;
use devworks_core::services::mcp::{McpClientService, McpEffectOutcome};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct RecordedRemote(
    AtomicUsize,
    bool,
    Option<Arc<super::TestClock>>,
    AtomicUsize,
    Mutex<Vec<bool>>,
    /// Disconnect calls.
    AtomicUsize,
);
impl McpRemote for RecordedRemote {
    fn connect(
        &self,
        _context: &InvocationContext,
        endpoint: &McpEndpoint,
    ) -> Result<(), McpRemoteFault> {
        self.0.fetch_add(1, Ordering::SeqCst);
        self.4.lock().unwrap().push(endpoint.sampling);
        if self.1 {
            Err(McpRemoteFault::MissingCredential)
        } else {
            Ok(())
        }
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
        if self.3.fetch_add(1, Ordering::SeqCst) == 0
            && let Some(clock) = &self.2
        {
            clock.0.store(super::T0 + 10_000, Ordering::SeqCst);
        }
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
        self.5.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}

fn artifacts(h: &Harness) -> Arc<ArtifactService> {
    Arc::new(ArtifactService::new(
        Arc::new(NativeArtifactFiles::new(
            h.root.0.clone(),
            h.root.0.with_extension("cache"),
        )),
        Arc::new(HostArtifactSources::new(h.clock.clone(), h.ids.clone())),
        Arc::new(NativeFilesystem::new()),
        h.clock.clone(),
        h.ids.clone(),
    ))
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
    let frame = json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":8,"requestKey":key,"body":body});
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
    let frame = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":8,"queryId":"mcp-list","body":{
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
    let frame = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":8,"queryId":"mcp-catalog","body":{
        "kind":"mcp.capabilities","scope":{"kind":"Workspace"},"serverId":server_id,
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
fn project_scoped_server_can_be_managed_from_workspace_context() {
    let h = Harness::new();
    let store = Arc::new(h.store());
    let project = ProjectId(id("mcp-project"));
    let project_for_write = project.clone();
    run_write(&*store, WriteOptions::default(), move |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")?;
        tx.projects().insert_project(&StoredProject {
            id: project_for_write,
            name: "MCP project".into(),
            description: String::new(),
            lifecycle: "Active".into(),
            revision: 1,
            scope_revision: 1,
            created_at_ms: 1,
            updated_at_ms: 1,
            archived_at_ms: None,
            deleted_at_ms: None,
            archive_epoch: 0,
            archive_stop_pending: false,
        })
    })
    .unwrap();
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    service
        .install_remote(
            store.clone(),
            Arc::new(RecordedRemote::default()),
            Arc::new(JsonSchemaValidator),
            artifacts(&h),
        )
        .unwrap();
    let cases = UseCases::new().with_mcp(service);
    let scope = json!({"kind":"Project","projectId":project});
    let created = command(
        &h,
        &store,
        &cases,
        "project-create",
        "mcp.configure",
        json!({
            "scope":scope,"name":"Project server",
            "transport":{"kind":"Http","endpoint":"https://mcp.example.test/mcp"}
        }),
    );
    let DomainDecision::Applied {
        result: OperationResult::McpConfigure { value: server },
    } = created
    else {
        panic!("project server configuration failed: {created:?}")
    };
    assert!(servers(&h, &store, &cases).is_empty());
    let connect = command(
        &h,
        &store,
        &cases,
        "project-connect",
        "mcp.connect",
        json!({
            "scope":scope,"serverId":server.id,"expectedRevision":"1"
        }),
    );
    assert!(matches!(connect, DomainDecision::Accepted { .. }));
    let select = command(
        &h,
        &store,
        &cases,
        "project-select",
        "mcp.select_tools",
        json!({
            "scope":scope,"serverId":server.id,"expectedRevision":"2",
            "expectedCatalogRevision":"1","toolIds":[]
        }),
    );
    assert!(matches!(
        select,
        DomainDecision::Applied {
            result: OperationResult::McpSelectTools { .. }
        }
    ));
    let project_for_write = project.clone();
    run_write(&*store, WriteOptions::default(), move |tx| {
        let mut row = tx
            .projects()
            .project(&project_for_write)?
            .expect("project row");
        row.lifecycle = "Archived".into();
        row.revision += 1;
        row.archive_epoch += 1;
        row.archived_at_ms = Some(2);
        tx.projects().update_project(&row, 1)
    })
    .unwrap();
    let rejected = command(
        &h,
        &store,
        &cases,
        "archived-project-connect",
        "mcp.connect",
        json!({
            "scope":scope,"serverId":server.id,"expectedRevision":"3"
        }),
    );
    assert!(matches!(rejected, DomainDecision::Refused { .. }));
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
        json!({"scope":{"kind":"Workspace"},"serverId":server.id,"expectedRevision":"1","expectedCatalogRevision":"1","toolIds":["unknown-tool"]}),
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
    // A newly added server starts with its read-only tools selected (MCP-06).
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
        json!({"scope":{"kind":"Workspace"},"serverId":server.id,"expectedRevision":"2","expectedCatalogRevision":"2","toolIds":["mcp-read","mcp-write"]}),
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
fn first_discovery_selects_read_only_hints_up_to_the_selection_bound() {
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
    let published = result.unwrap();
    assert_eq!(published.catalog_revision.get(), 2);
    assert_eq!(published.selected_tool_ids.0.len(), 128);
    assert_eq!(published.selected_tool_ids.0[0], id("read-0"));
    assert!(!published.selected_tool_ids.0.contains(&id("read-128")));
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
        .install_remote(
            store.clone(),
            remote.clone(),
            Arc::new(JsonSchemaValidator),
            artifacts(&h),
        )
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
        json!({"scope":{"kind":"Workspace"},"serverId":server.id,"expectedRevision":"1"}),
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
        json!({"scope":{"kind":"Workspace"},"serverId":server.id,"expectedRevision":"3"}),
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
fn failed_connection_persists_a_safe_diagnostic_for_settings() {
    let h = Harness::new();
    let store = Arc::new(h.store());
    run_write(&*store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")
    })
    .unwrap();
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    service
        .install_remote(
            store.clone(),
            Arc::new(RecordedRemote(
                AtomicUsize::new(0),
                true,
                None,
                AtomicUsize::new(0),
                Mutex::default(),
                AtomicUsize::new(0),
            )),
            Arc::new(JsonSchemaValidator),
            artifacts(&h),
        )
        .unwrap();
    let cases = UseCases::new().with_mcp(service.clone());
    let DomainDecision::Applied {
        result: OperationResult::McpConfigure { value: server },
    } = command(
        &h,
        &store,
        &cases,
        "diagnostic-configure",
        "mcp.configure",
        json!({
            "scope":{"kind":"Workspace"},"name":"Credential server",
            "transport":{"kind":"Http","endpoint":"https://mcp.example.test/mcp"}
        }),
    )
    else {
        panic!("server configuration failed")
    };
    let DomainDecision::Accepted { operation_id } = command(
        &h,
        &store,
        &cases,
        "diagnostic-connect",
        "mcp.connect",
        json!({
            "scope":{"kind":"Workspace"},"serverId":server.id,"expectedRevision":"1"
        }),
    ) else {
        panic!("connection was not accepted")
    };
    let operation = dispatched(&store, &operation_id);
    assert!(matches!(
        service.execute(&operation),
        McpEffectOutcome::Failed(_)
    ));
    let server = servers(&h, &store, &cases).remove(0);
    assert!(matches!(
        server.state,
        devworks_core::protocol::records::ItemState::Failed
    ));
    assert_eq!(
        server.last_failure.expect("failure diagnostic").0,
        "Connection failed: MissingCredential"
    );
    let DomainDecision::Accepted { operation_id } = command(
        &h,
        &store,
        &cases,
        "diagnostic-reconnect",
        "mcp.connect",
        json!({
            "scope":{"kind":"Workspace"},"serverId":server.id,"expectedRevision":"3"
        }),
    ) else {
        panic!("reconnection was not accepted")
    };
    let operation = dispatched(&store, &operation_id);
    assert!(matches!(
        service.observe(&operation),
        McpEffectOutcome::Unknown(_)
    ));
    let server = servers(&h, &store, &cases).remove(0);
    assert!(matches!(
        server.state,
        devworks_core::protocol::records::ItemState::Failed
    ));
    assert!(
        server
            .last_failure
            .expect("recovery diagnostic")
            .0
            .contains("could not be confirmed")
    );
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
    let first_cap = match capabilities(&h, &store, &cases, &server.id, 2) {
        DomainQueryReply::Result {
            result: OperationResult::McpCapabilities { value },
            ..
        } => value
            .items
            .0
            .into_iter()
            .find(|cap| cap.kind.0 == "tool")
            .unwrap(),
        other => panic!("unexpected catalog: {other:?}"),
    };
    assert!(first_cap.input_schema.is_some());
    let DomainDecision::Applied {
        result: OperationResult::McpSelectTools {
            value: selected_server,
        },
    } = command(
        &h,
        &store,
        &cases,
        "catalog-select",
        "mcp.select_tools",
        json!({"scope":{"kind":"Workspace"},"serverId":server.id,
            "expectedRevision":"2","expectedCatalogRevision":"2",
            "toolIds":[first_cap.id.clone()]}),
    )
    else {
        panic!("human selection failed");
    };
    let selected = read_snapshot(&*store, &ReadOptions::default(), |snapshot| {
        service.selected_tool_materials(&context(&store), snapshot)
    })
    .unwrap();
    assert_eq!(selected.len(), 1);
    assert!(selected[0].alias.wire_name.len() <= 64);
    assert_eq!(selected[0].schema["type"], "object");
    assert_eq!(
        selected[0].alias.capability_id.as_str().split('/').next(),
        Some("mcp")
    );
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
    assert_eq!(
        second.selected_tool_ids.0,
        selected_server.selected_tool_ids.0
    );
    assert_eq!(second.revision, selected_server.revision);
    assert_eq!(second.catalog_revision, selected_server.catalog_revision);
    let second_cap = match capabilities(&h, &store, &cases, &server.id, 2) {
        DomainQueryReply::Result {
            result: OperationResult::McpCapabilities { value },
            ..
        } => value
            .items
            .0
            .into_iter()
            .find(|cap| cap.kind.0 == "tool")
            .unwrap(),
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
                &Revision::new(2).unwrap(),
                catalog(json!({"type":"object","properties":{"key":{"type":"number"}}})),
            )
        })
        .unwrap()
    };
    assert!(changed.selected_tool_ids.0.is_empty());
    let oversized = {
        let service = service.clone();
        let server_id = server.id.clone();
        run_write(&*store, WriteOptions::default(), move |tx| {
            service.publish_remote_catalog(
                tx,
                &ConfigScope::Workspace,
                &server_id,
                &Revision::new(3).unwrap(),
                RemoteCatalog {
                    tools: (0..20)
                        .map(|index| RemoteTool {
                            name: format!("large-{index}"),
                            description: "x".repeat(65_000),
                            input_schema: json!({"type":"object"}),
                            output_schema: None,
                            read_only_hint: false,
                        })
                        .collect(),
                    resources: vec![],
                    prompts: vec![],
                },
            )
        })
    };
    assert!(oversized.is_err(), "oversized catalog was retained");
    // The host sampling entry does not take one of the 500 remote catalog slots.
    let tools = |count: usize| RemoteCatalog {
        tools: (0..count)
            .map(|index| RemoteTool {
                name: format!("bounded-{index}"),
                description: String::new(),
                input_schema: json!({"type":"object"}),
                output_schema: None,
                read_only_hint: false,
            })
            .collect(),
        resources: vec![],
        prompts: vec![],
    };
    for (count, accepted) in [(501, false), (500, true)] {
        let service = service.clone();
        let server_id = server.id.clone();
        let published = run_write(&*store, WriteOptions::default(), move |tx| {
            service.publish_remote_catalog(
                tx,
                &ConfigScope::Workspace,
                &server_id,
                &Revision::new(3).unwrap(),
                tools(count),
            )
        });
        assert_eq!(published.is_ok(), accepted, "{count} remote entries");
    }
}

#[test]
fn journaled_first_discovery_selects_read_only_hints() {
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
        .install_remote(
            store.clone(),
            remote,
            Arc::new(JsonSchemaValidator),
            artifacts(&h),
        )
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
        json!({"scope":{"kind":"Workspace"},"serverId":server.id,"expectedRevision":"1"}),
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
        json!({"scope":{"kind":"Workspace"},"serverId":server.id,"expectedCatalogRevision":"1"}),
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
    let before = read_snapshot(&*store, &ReadOptions::default(), |snapshot| {
        Ok(snapshot
            .configuration()
            .current(&ConfigScope::Workspace, "outgoing_mcp")?
            .unwrap()
            .revision)
    })
    .unwrap();
    let DomainDecision::Accepted { operation_id } = command(
        &h,
        &store,
        &cases,
        "discovery-unchanged",
        "mcp.discover",
        json!({"scope":{"kind":"Workspace"},"serverId":server.id,"expectedCatalogRevision":"2"}),
    ) else {
        panic!("repeat discovery was not accepted");
    };
    let operation = dispatched(&store, &operation_id);
    assert!(matches!(
        service.execute(&operation),
        McpEffectOutcome::Completed(_)
    ));
    let after = read_snapshot(&*store, &ReadOptions::default(), |snapshot| {
        Ok(snapshot
            .configuration()
            .current(&ConfigScope::Workspace, "outgoing_mcp")?
            .unwrap()
            .revision)
    })
    .unwrap();
    assert_eq!(
        after, before,
        "unchanged discovery retained a full snapshot"
    );
    let selected = read_snapshot(&*store, &ReadOptions::default(), |snapshot| {
        service.selected_tool_materials(&context(&store), snapshot)
    })
    .unwrap();
    assert_eq!(
        selected
            .iter()
            .map(|material| material.wire_name.as_str())
            .collect::<Vec<_>>(),
        ["lookup"]
    );
    assert!(
        matches!(capabilities(&h, &store, &cases, &server.id, 2), DomainQueryReply::Result { result: OperationResult::McpCapabilities { value }, .. } if value.items.0.len() == 2)
    );
}

#[test]
fn resource_and_prompt_queries_publish_bounded_untrusted_content() {
    let h = Harness::new();
    let store = Arc::new(h.store());
    run_write(&*store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")?;
        Ok(())
    })
    .unwrap();
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    service
        .install_remote(
            store.clone(),
            Arc::new(RecordedRemote::default()),
            Arc::new(JsonSchemaValidator),
            artifacts(&h),
        )
        .unwrap();
    let cases = UseCases::new().with_mcp(service.clone());
    let DomainDecision::Applied {
        result: OperationResult::McpConfigure { value: server },
    } = command(
        &h,
        &store,
        &cases,
        "content-server",
        "mcp.configure",
        json!({"scope":{"kind":"Workspace"},"name":"Content server",
            "transport":{"kind":"Http","endpoint":"https://mcp.example.test/mcp"}}),
    )
    else {
        panic!("MCP server configuration failed");
    };
    let server_id = server.id.clone();
    let service_for_write = service.clone();
    run_write(&*store, WriteOptions::default(), move |tx| {
        service_for_write.publish_remote_catalog(
            tx,
            &ConfigScope::Workspace,
            &server_id,
            &Revision::new(1).unwrap(),
            RemoteCatalog {
                tools: vec![],
                resources: vec![RemoteResource {
                    uri: "mcp://example/readme".into(),
                    name: "Readme".into(),
                    description: "A test resource".into(),
                }],
                prompts: vec![RemotePrompt {
                    name: "review".into(),
                    description: "Review a change".into(),
                    arguments: vec![RemotePromptArgument {
                        name: "change".into(),
                        required: true,
                    }],
                }],
            },
        )
    })
    .unwrap();
    let authority = HostAuthority {
        store: &*store,
        clock: &*h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        authority: &authority,
        store: &*store,
        ids: &*h.ids,
    };
    let query = |operation: &str, body: Value| {
        let frame = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":8,"queryId":"mcp-content-read",
            "body":body});
        cases
            .query(
                &gateway,
                &context(&store),
                operation,
                frame.to_string().as_bytes(),
            )
            .unwrap()
    };
    let resources = query(
        "mcp.resources",
        json!({"kind":"mcp.resources","scope":{"kind":"Workspace"},"serverId":server.id,
        "catalogRevision":"2","page":{"limit":10}}),
    );
    let DomainQueryReply::Result {
        result: OperationResult::McpResources { value },
        ..
    } = resources
    else {
        panic!("MCP resource read failed: {resources:?}");
    };
    assert_eq!(value.items.0.len(), 1);
    assert_eq!(value.items.0[0].uri.0, "mcp://example/readme");
    let resource = value.items.0[0].content.clone();
    let missing_argument = query(
        "mcp.prompts",
        json!({"kind":"mcp.prompts","scope":{"kind":"Workspace"},"serverId":server.id,
        "catalogRevision":"2","page":{"limit":10}}),
    );
    assert!(matches!(
        missing_argument,
        DomainQueryReply::Rejected { .. }
    ));
    let reference = match capabilities(&h, &store, &cases, &server.id, 2) {
        DomainQueryReply::Result {
            result: OperationResult::McpCapabilities { value },
            ..
        } => value
            .items
            .0
            .into_iter()
            .find(|capability| capability.kind.0 == "prompt")
            .and_then(|capability| capability.input_schema)
            .expect("prompt schema"),
        other => panic!("MCP prompt schema unavailable: {other:?}"),
    };
    let prompts = query(
        "mcp.prompts",
        json!({"kind":"mcp.prompts","scope":{"kind":"Workspace"},"serverId":server.id,
            "catalogRevision":"2","name":"review",
            "arguments":{"schema":reference,"value":{"change":"example"}},
            "page":{"limit":10}}),
    );
    let DomainQueryReply::Result {
        result: OperationResult::McpPrompts { value },
        ..
    } = prompts
    else {
        panic!("MCP prompt read failed: {prompts:?}");
    };
    assert_eq!(value.items.0.len(), 1);
    assert_eq!(value.items.0[0].name.0, "review");
    let prompt = value.items.0[0].messages.clone();
    read_snapshot(&*store, &ReadOptions::default(), |snapshot| {
        assert!(
            snapshot
                .artifacts()
                .version(&resource.version_id)?
                .is_some_and(|v| v.live())
        );
        assert!(
            snapshot
                .artifacts()
                .version(&prompt.version_id)?
                .is_some_and(|v| v.live())
        );
        Ok(())
    })
    .unwrap();
}

#[test]
fn resource_query_stops_when_deadline_expires_during_a_remote_read() {
    let h = Harness::new();
    let store = Arc::new(h.store());
    run_write(&*store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")?;
        Ok(())
    })
    .unwrap();
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    let remote = Arc::new(RecordedRemote(
        AtomicUsize::new(0),
        false,
        Some(h.clock.clone()),
        AtomicUsize::new(0),
        Mutex::default(),
        AtomicUsize::new(0),
    ));
    service
        .install_remote(
            store.clone(),
            remote.clone(),
            Arc::new(JsonSchemaValidator),
            artifacts(&h),
        )
        .unwrap();
    let cases = UseCases::new().with_mcp(service.clone());
    let DomainDecision::Applied {
        result: OperationResult::McpConfigure { value: server },
    } = command(
        &h,
        &store,
        &cases,
        "deadline-server",
        "mcp.configure",
        json!({"scope":{"kind":"Workspace"},"name":"Deadline server",
            "transport":{"kind":"Http","endpoint":"https://mcp.example.test/mcp"}}),
    )
    else {
        panic!("MCP server configuration failed");
    };
    let server_id = server.id.clone();
    let service_for_write = service.clone();
    run_write(&*store, WriteOptions::default(), move |tx| {
        service_for_write.publish_remote_catalog(
            tx,
            &ConfigScope::Workspace,
            &server_id,
            &Revision::new(1).unwrap(),
            RemoteCatalog {
                tools: vec![],
                resources: ["mcp://example/one", "mcp://example/two"]
                    .into_iter()
                    .map(|uri| RemoteResource {
                        uri: uri.into(),
                        name: uri.into(),
                        description: String::new(),
                    })
                    .collect(),
                prompts: vec![],
            },
        )
    })
    .unwrap();
    let authority = HostAuthority {
        store: &*store,
        clock: &*h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        authority: &authority,
        store: &*store,
        ids: &*h.ids,
    };
    let frame = json!({"mode":"Query","protocol":"1.0-draft.1","schemaRevision":8,"queryId":"deadline-read",
        "body":{"kind":"mcp.resources","scope":{"kind":"Workspace"},
            "serverId":server.id,"catalogRevision":"2","page":{"limit":10}}});
    let reply = gateway
        .query_with(
            &context(&store),
            "mcp.resources",
            frame.to_string().as_bytes(),
            service.as_ref(),
            ReadOptions {
                deadline_ms: Some(super::T0 + 5_000),
                ..ReadOptions::default()
            },
        )
        .unwrap();
    assert!(matches!(reply, DomainQueryReply::Rejected { .. }));
    assert_eq!(remote.3.load(Ordering::SeqCst), 1);
}

fn sampling_capability(h: &Harness, store: &SqliteStore, cases: &UseCases, server: &Id) -> Id {
    match capabilities(h, store, cases, server, 2) {
        DomainQueryReply::Result {
            result: OperationResult::McpCapabilities { value },
            ..
        } => {
            let sampling: Vec<_> = value
                .items
                .0
                .into_iter()
                .filter(|cap| cap.kind.0 == "sampling")
                .collect();
            assert_eq!(sampling.len(), 1);
            assert_eq!(sampling[0].name.0, "createMessage");
            assert!(sampling[0].input_schema.is_none());
            sampling[0].id.clone()
        }
        other => panic!("unexpected catalog: {other:?}"),
    }
}

/// A connected and discovered workspace server. Returns its ID and current revision.
fn discovered_server(
    h: &Harness,
    store: &SqliteStore,
    cases: &UseCases,
    service: &Arc<McpClientService>,
) -> (Id, i64) {
    let DomainDecision::Applied {
        result: OperationResult::McpConfigure { value: server },
    } = command(
        h,
        store,
        cases,
        "sampling-server",
        "mcp.configure",
        json!({"scope":{"kind":"Workspace"},"name":"Sampling server",
            "transport":{"kind":"Http","endpoint":"https://mcp.example.test/mcp"}}),
    )
    else {
        panic!("server configuration failed");
    };
    for (key, operation, body) in [
        (
            "sampling-connect",
            "mcp.connect",
            json!({"expectedRevision":"1"}),
        ),
        (
            "sampling-discover",
            "mcp.discover",
            json!({"expectedCatalogRevision":"1"}),
        ),
    ] {
        let mut body = body;
        body["scope"] = json!({"kind":"Workspace"});
        body["serverId"] = json!(server.id);
        let DomainDecision::Accepted { operation_id } =
            command(h, store, cases, key, operation, body)
        else {
            panic!("{operation} was not accepted");
        };
        assert!(matches!(
            service.execute(&dispatched(store, &operation_id)),
            McpEffectOutcome::Completed(_)
        ));
    }
    let current = servers(h, store, cases).remove(0);
    assert_eq!(current.catalog_revision.get(), 2);
    (server.id, current.revision.get())
}

fn select(
    h: &Harness,
    store: &SqliteStore,
    cases: &UseCases,
    key: &str,
    server: &Id,
    ids: Vec<Id>,
) -> i64 {
    let revision = servers(h, store, cases)[0].revision.get();
    let DomainDecision::Applied {
        result: OperationResult::McpSelectTools { value },
    } = command(
        h,
        store,
        cases,
        key,
        "mcp.select_tools",
        json!({"scope":{"kind":"Workspace"},"serverId":server,
            "expectedRevision":revision.to_string(),"expectedCatalogRevision":"2","toolIds":ids}),
    )
    else {
        panic!("selection failed");
    };
    value.revision.get()
}

#[test]
fn sampling_selection_is_advertised_survives_rediscovery_and_clears_on_transport_change() {
    let h = Harness::new();
    let store = Arc::new(h.store());
    run_write(&*store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")
    })
    .unwrap();
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    let remote = Arc::new(RecordedRemote::default());
    service
        .install_remote(
            store.clone(),
            remote.clone(),
            Arc::new(JsonSchemaValidator),
            artifacts(&h),
        )
        .unwrap();
    let cases = UseCases::new().with_mcp(service.clone());
    let (server, _) = discovered_server(&h, &store, &cases, &service);
    let sampling = sampling_capability(&h, &store, &cases, &server);
    // Only the read-only tool starts selected; sampling is never selected by default.
    assert!(
        !servers(&h, &store, &cases)[0]
            .selected_tool_ids
            .0
            .contains(&sampling)
    );
    let revision = select(
        &h,
        &store,
        &cases,
        "select-sampling",
        &server,
        vec![sampling.clone()],
    );
    // The sampling entry is never a provider tool.
    let materials = read_snapshot(&*store, &ReadOptions::default(), |snapshot| {
        service.selected_tool_materials(&context(&store), snapshot)
    })
    .unwrap();
    assert!(materials.is_empty());
    let DomainDecision::Accepted { operation_id } = command(
        &h,
        &store,
        &cases,
        "sampling-reconnect",
        "mcp.connect",
        json!({"scope":{"kind":"Workspace"},"serverId":server,
            "expectedRevision":revision.to_string()}),
    ) else {
        panic!("reconnect was not accepted");
    };
    service.execute(&dispatched(&store, &operation_id));
    assert_eq!(*remote.4.lock().unwrap(), [false, false, true]);
    let DomainDecision::Accepted { operation_id } = command(
        &h,
        &store,
        &cases,
        "sampling-rediscover",
        "mcp.discover",
        json!({"scope":{"kind":"Workspace"},"serverId":server,"expectedCatalogRevision":"2"}),
    ) else {
        panic!("rediscovery was not accepted");
    };
    service.execute(&dispatched(&store, &operation_id));
    let current = servers(&h, &store, &cases).remove(0);
    assert_eq!(current.catalog_revision.get(), 2);
    assert_eq!(current.selected_tool_ids.0, vec![sampling.clone()]);
    assert_eq!(remote.4.lock().unwrap().last(), Some(&true));
    let changed = command(
        &h,
        &store,
        &cases,
        "sampling-transport",
        "mcp.configure",
        json!({"scope":{"kind":"Workspace"},"serverId":server,
            "expectedRevision":current.revision.get().to_string(),"name":"Sampling server",
            "transport":{"kind":"Http","endpoint":"https://other.example.test/mcp"}}),
    );
    assert!(matches!(changed, DomainDecision::Applied { .. }));
    assert!(
        servers(&h, &store, &cases)[0]
            .selected_tool_ids
            .0
            .is_empty()
    );
}

fn agent_session(store: &SqliteStore, session: &str, closed: bool) {
    let session = id(session);
    run_write(store, WriteOptions::default(), move |tx| {
        tx.sessions()
            .insert_session(&devworks_core::ports::storage::StoredSession {
                id: session.clone(),
                conversation_id: session,
                agent_id: ActorId(id("mcp-agent")),
                project_id: None,
                root_id: None,
                title: "Agent session".into(),
                context_epoch: 0,
                revision: 1,
                created_by: ActorId(id("mcp-user")),
                created_at_ms: 1,
                closed_at_ms: closed.then_some(2),
                closed_by: closed.then(|| ActorId(id("mcp-user"))),
                next_sequence: 1,
                checkpoint_sequence: 0,
            })
    })
    .unwrap();
}

fn bind_agent(store: &SqliteStore, expected: Option<i64>) -> i64 {
    let by = context(store);
    run_write(store, WriteOptions::default(), move |tx| {
        devworks_core::authority::service::bind_principal(
            tx,
            &by,
            devworks_core::ports::storage::NewPrincipalBinding {
                actor: ActorId(id("mcp-agent")),
                kind: PrincipalKind::Agent,
                role: Some(devworks_core::settings::catalog::AgentRole::Worker),
                project_id: None,
                tool_families: None,
                bound_by: ActorId(id("mcp-user")),
            },
            expected,
        )
        .map(|binding| binding.revision)
        .map_err(|e| devworks_core::ports::storage::StorageError::InvalidRecord(format!("{e:?}")))
    })
    .unwrap()
}

/// An attributed `mcp.tool` journal as `queue_selected_tool` records it.
fn tool_call(
    store: &SqliteStore,
    name: &str,
    server: &Id,
    edit: impl FnOnce(&mut Value),
    dispatch: bool,
) -> devworks_core::ports::mcp::McpCallOrigin {
    let revision = read_snapshot(store, &ReadOptions::default(), |s| {
        Ok(s.authority()
            .principal(&ActorId(id("mcp-agent")))?
            .unwrap()
            .revision)
    })
    .unwrap();
    let mut payload = json!({
        "scope":ConfigScope::Workspace.key(),"serverId":server,"serverRevision":1,
        "catalogRevision":2,"principal":"mcp-agent","principalKind":"Agent",
        "sessionId":"agent-session","projectId":null,
        "fences":[devworks_core::ports::AuthorityFence::Principal {
            actor: ActorId(id("mcp-agent")), revision }],
        "call":{"attemptId":"provider-attempt","nativeCallId":name,"ordinal":"0"},
    });
    edit(&mut payload);
    let operation = OperationId(id(name));
    let intent = operation.clone();
    run_write(store, WriteOptions::default(), move |tx| {
        tx.operations()
            .record_intent(devworks_core::ports::storage::NewOperation {
                id: intent,
                root_id: None,
                assignment_id: None,
                capability: "mcp.tool".into(),
                schema_version: 1,
                args_digest: "a".repeat(64),
                payload,
            })
            .map(|_| ())
    })
    .unwrap();
    let attempt = if dispatch {
        dispatched(store, &operation)
            .latest_attempt()
            .unwrap()
            .id
            .0
            .clone()
    } else {
        id("journal-attempt")
    };
    devworks_core::ports::mcp::McpCallOrigin {
        server_id: server.clone(),
        operation_id: operation,
        attempt_id: attempt,
    }
}

#[test]
fn sampling_is_authorized_only_for_the_dispatching_agent_call_with_current_selection() {
    use devworks_core::ports::mcp::SamplingRefusal::Unauthorized;
    let h = Harness::new();
    let store = Arc::new(h.store());
    run_write(&*store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")?;
        tx.actors()
            .register(&ActorId(id("mcp-agent")), PrincipalKind::Agent, "MCP agent")
    })
    .unwrap();
    bind_agent(&store, None);
    agent_session(&store, "agent-session", false);
    agent_session(&store, "closed-session", true);
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    service
        .install_remote(
            store.clone(),
            Arc::new(RecordedRemote::default()),
            Arc::new(JsonSchemaValidator),
            artifacts(&h),
        )
        .unwrap();
    let cases = UseCases::new().with_mcp(service.clone());
    let (server, _) = discovered_server(&h, &store, &cases, &service);
    let sampling = sampling_capability(&h, &store, &cases, &server);
    let call = tool_call(&store, "mcp-call", &server, |_| {}, true);
    assert_eq!(
        service.authorize_sampling(&call, 1).err(),
        Some(Unauthorized("mcp_sampling_selection"))
    );
    select(
        &h,
        &store,
        &cases,
        "select-sampling",
        &server,
        vec![sampling],
    );
    let grant = service
        .authorize_sampling(&call, 2)
        .expect("selected agent call");
    assert_eq!(grant.context.principal(), &ActorId(id("mcp-agent")));
    assert_eq!(grant.context.principal_kind(), PrincipalKind::Agent);
    assert_eq!(grant.context.session_id(), Some(&id("agent-session")));
    assert_eq!(grant.operation_id, call.operation_id);
    assert_eq!(grant.call.native_call_id, "mcp-call");
    let other_attempt = devworks_core::ports::mcp::McpCallOrigin {
        attempt_id: id("other-attempt"),
        ..call.clone()
    };
    let other_server = devworks_core::ports::mcp::McpCallOrigin {
        server_id: id("other-server"),
        ..call.clone()
    };
    let queued = tool_call(&store, "queued-call", &server, |_| {}, false);
    let host = tool_call(
        &store,
        "host-call",
        &server,
        |p| p["principalKind"] = json!("Host"),
        true,
    );
    let closed = tool_call(
        &store,
        "closed-call",
        &server,
        |p| p["sessionId"] = json!("closed-session"),
        true,
    );
    let stale = tool_call(
        &store,
        "stale-call",
        &server,
        |p| p["catalogRevision"] = json!(1),
        true,
    );
    for (origin, reason) in [
        (&other_attempt, "mcp_sampling_call"),
        (&other_server, "mcp_sampling_call"),
        (&queued, "mcp_sampling_call"),
        (&host, "mcp_sampling_principal"),
        (&closed, "mcp_sampling_session"),
        (&stale, "mcp_sampling_selection"),
    ] {
        assert_eq!(
            service.authorize_sampling(origin, 1).err(),
            Some(Unauthorized(reason))
        );
    }
    let revision = read_snapshot(&*store, &ReadOptions::default(), |s| {
        Ok(s.authority()
            .principal(&ActorId(id("mcp-agent")))?
            .unwrap()
            .revision)
    })
    .unwrap();
    bind_agent(&store, Some(revision));
    assert_eq!(
        service.authorize_sampling(&call, 3).err(),
        Some(Unauthorized("mcp_sampling_authority"))
    );
    let kinds = read_snapshot(&*store, &ReadOptions::default(), |s| {
        Ok(s.history()
            .page(
                &devworks_core::ports::storage::EventFilter::Aggregate {
                    kind: "mcp_tool_call".into(),
                    id: "mcp-call".into(),
                },
                0,
                50,
            )?
            .events
            .into_iter()
            .map(|event| (event.kind, event.payload["reason"].clone()))
            .collect::<Vec<_>>())
    })
    .unwrap();
    assert_eq!(
        kinds,
        [
            (
                "McpSamplingRefused".to_owned(),
                json!("mcp_sampling_selection")
            ),
            ("McpSamplingAccepted".to_owned(), Value::Null),
            ("McpSamplingRefused".to_owned(), json!("mcp_sampling_call")),
            ("McpSamplingRefused".to_owned(), json!("mcp_sampling_call")),
            (
                "McpSamplingRefused".to_owned(),
                json!("mcp_sampling_authority")
            ),
        ]
    );
}

#[test]
fn a_vanished_selected_tool_stays_unavailable_and_returns_selected() {
    let h = Harness::new();
    let store = Arc::new(h.store());
    run_write(&*store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")
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
        "vanish-server",
        "mcp.configure",
        json!({"scope":{"kind":"Workspace"},"name":"Vanishing server",
            "transport":{"kind":"Http","endpoint":"https://mcp.example.test/mcp"}}),
    )
    else {
        panic!("server configuration failed");
    };
    let tool = |name: &str, read_only_hint| RemoteTool {
        name: name.into(),
        description: format!("{name} a record"),
        input_schema: json!({"type":"object"}),
        output_schema: None,
        read_only_hint,
    };
    let publish = |expected: i64, tools: Vec<RemoteTool>| {
        let (service, server_id) = (service.clone(), server.id.clone());
        run_write(&*store, WriteOptions::default(), move |tx| {
            service.publish_remote_catalog(
                tx,
                &ConfigScope::Workspace,
                &server_id,
                &Revision::new(expected).unwrap(),
                RemoteCatalog {
                    tools,
                    resources: vec![],
                    prompts: vec![],
                },
            )
        })
        .unwrap()
    };
    let tool_id =
        |catalog: i64, name: &str| match capabilities(&h, &store, &cases, &server.id, catalog) {
            DomainQueryReply::Result {
                result: OperationResult::McpCapabilities { value },
                ..
            } => value
                .items
                .0
                .into_iter()
                .find(|cap| cap.kind.0 == "tool" && cap.name.0 == name)
                .unwrap(),
            other => panic!("unexpected catalog: {other:?}"),
        };
    let offered = || {
        read_snapshot(&*store, &ReadOptions::default(), |snapshot| {
            service.selected_tool_materials(&context(&store), snapshot)
        })
        .unwrap()
        .into_iter()
        .map(|material| material.wire_name)
        .collect::<Vec<_>>()
    };

    // A newly added server starts with only its read-only tool selected.
    let added = publish(1, vec![tool("lookup", true), tool("write", false)]);
    let (lookup, write) = (tool_id(2, "lookup").id, tool_id(2, "write").id);
    assert_eq!(added.selected_tool_ids.0, vec![lookup.clone()]);

    // The human deselects the read-only tool and selects the other one.
    let DomainDecision::Applied { .. } = command(
        &h,
        &store,
        &cases,
        "vanish-select",
        "mcp.select_tools",
        json!({"scope":{"kind":"Workspace"},"serverId":server.id,"expectedRevision":"2",
            "expectedCatalogRevision":"2","toolIds":[write]}),
    ) else {
        panic!("selection failed");
    };

    // The selected tool vanishes: it stays listed without a schema, stays selected and is not
    // offered. The deselected read-only tool is not selected again.
    let vanished = publish(2, vec![tool("lookup", true)]);
    assert_eq!(vanished.selected_tool_ids.0, vec![write.clone()]);
    let unavailable = tool_id(3, "write");
    assert_eq!(unavailable.id, write);
    assert!(unavailable.input_schema.is_none());
    assert!(offered().is_empty());
    // The same discovery again, with the tool still absent, changes nothing.
    let repeated = publish(3, vec![tool("lookup", true)]);
    assert_eq!(repeated.catalog_revision.get(), 3);
    assert_eq!(repeated.revision, vanished.revision);
    assert_eq!(tool_id(3, "write").revision, unavailable.revision);

    // It returns unchanged and is offered again under its original selection.
    let returned = publish(3, vec![tool("lookup", true), tool("write", false)]);
    assert_eq!(returned.selected_tool_ids.0, vec![write.clone()]);
    assert!(tool_id(4, "write").input_schema.is_some());
    assert_eq!(offered(), ["write"]);

    // A tool that returns changed is not reselected.
    let vanished_again = publish(4, vec![tool("lookup", true)]);
    assert_eq!(vanished_again.selected_tool_ids.0, vec![write.clone()]);
    let mut changed = tool("write", false);
    changed.description = "Write a different record".into();
    let changed = publish(5, vec![tool("lookup", true), changed]);
    assert!(changed.selected_tool_ids.0.is_empty());
}

#[test]
fn a_reviewer_is_offered_no_default_selected_mcp_tool() {
    use devworks_core::authority::service::bind_principal;
    use devworks_core::ports::storage::NewPrincipalBinding;
    use devworks_core::settings::{catalog, catalog::AgentRole, model};
    let h = Harness::new();
    let store = Arc::new(h.store());
    run_write(&*store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")?;
        for agent in ["mcp-worker", "mcp-reviewer"] {
            tx.actors()
                .register(&ActorId(id(agent)), PrincipalKind::Agent, agent)?;
        }
        Ok(())
    })
    .unwrap();
    let human = context(&store);
    for (agent, role) in [
        ("mcp-worker", AgentRole::Worker),
        ("mcp-reviewer", AgentRole::CodeReviewer),
    ] {
        let human = human.clone();
        run_write(&*store, WriteOptions::default(), move |tx| {
            bind_principal(
                tx,
                &human,
                NewPrincipalBinding {
                    actor: ActorId(id(agent)),
                    kind: PrincipalKind::Agent,
                    role: Some(role),
                    project_id: None,
                    tool_families: None,
                    bound_by: ActorId(id("mcp-user")),
                },
                None,
            )
            .map_err(|error| {
                devworks_core::ports::storage::StorageError::InvalidRecord(format!("{error:?}"))
            })
        })
        .unwrap();
    }
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    let cases = UseCases::new().with_mcp(service.clone());
    let DomainDecision::Applied {
        result: OperationResult::McpConfigure { value: server },
    } = command(
        &h,
        &store,
        &cases,
        "reviewer-server",
        "mcp.configure",
        json!({"scope":{"kind":"Workspace"},"name":"Read server",
            "transport":{"kind":"Http","endpoint":"https://mcp.example.test/mcp"}}),
    )
    else {
        panic!("server configuration failed");
    };
    let (service_for_write, server_id) = (service.clone(), server.id.clone());
    let published = run_write(&*store, WriteOptions::default(), move |tx| {
        service_for_write.publish_remote_catalog(
            tx,
            &ConfigScope::Workspace,
            &server_id,
            &Revision::new(1).unwrap(),
            RemoteCatalog {
                tools: vec![RemoteTool {
                    name: "lookup".into(),
                    description: "Read a record".into(),
                    input_schema: json!({"type":"object"}),
                    output_schema: None,
                    read_only_hint: true,
                }],
                resources: vec![],
                prompts: vec![],
            },
        )
    })
    .unwrap();
    let tool = published.selected_tool_ids.0[0].clone();
    // Both agents' tool settings allow the tool, so only the role decides.
    let capability = format!("mcp/{}", tool.as_str().strip_prefix("mcp-").unwrap());
    run_write(&*store, WriteOptions::default(), move |tx| {
        let current = model::current_layer(tx.configuration(), &ConfigScope::Workspace)?;
        let descriptor = catalog::descriptor(catalog::TOOL_OVERRIDES_KEY).unwrap();
        let assignment = serde_json::from_value(json!({"key": descriptor.key,
            "value": {"schema": descriptor.schema_ref(), "value": {(capability): true}}}))
        .unwrap();
        let plan = model::plan(&current, &[assignment]);
        let body = model::body(
            &plan,
            &ActorId(id("mcp-user")),
            tx.now_ms(),
            json!({"kind":"test"}),
        );
        tx.configuration().append_revision(
            &ConfigScope::Workspace,
            model::NAMESPACE,
            current.revision,
            model::BODY_SCHEMA_VERSION,
            body,
        )
    })
    .unwrap();
    let offered = |agent: &str| {
        let ctx = HostIngress::new(&*store).context(
            ActorId(id(agent)),
            PrincipalKind::Agent,
            id("mcp-invocation"),
            id("mcp-agent"),
            Counter::new(store.info().restore_epoch).unwrap(),
            Revision::new(1).unwrap(),
        );
        read_snapshot(&*store, &ReadOptions::default(), |snapshot| {
            service.selected_tool_materials(&ctx, snapshot)
        })
        .unwrap()
        .len()
    };
    assert_eq!(offered("mcp-worker"), 1);
    assert_eq!(offered("mcp-reviewer"), 0);
}

#[test]
fn two_servers_with_the_same_tool_name_get_distinct_agent_aliases() {
    let h = Harness::new();
    let store = Arc::new(h.store());
    run_write(&*store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")
    })
    .unwrap();
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    let cases = UseCases::new().with_mcp(service.clone());
    let mut servers = Vec::new();
    for name in ["First server", "Second server"] {
        let DomainDecision::Applied {
            result: OperationResult::McpConfigure { value: server },
        } = command(
            &h,
            &store,
            &cases,
            &format!("collision-{}", name.replace(' ', "-")),
            "mcp.configure",
            json!({"scope":{"kind":"Workspace"},"name":name,
                "transport":{"kind":"Http","endpoint":format!(
                    "https://{}.example.test/mcp", name.replace(' ', "-").to_lowercase())}}),
        )
        else {
            panic!("server configuration failed");
        };
        let (service_for_write, server_id) = (service.clone(), server.id.clone());
        run_write(&*store, WriteOptions::default(), move |tx| {
            service_for_write.publish_remote_catalog(
                tx,
                &ConfigScope::Workspace,
                &server_id,
                &Revision::new(1).unwrap(),
                RemoteCatalog {
                    tools: vec![RemoteTool {
                        name: "lookup".into(),
                        description: "Read a record".into(),
                        input_schema: json!({"type":"object"}),
                        output_schema: None,
                        read_only_hint: false,
                    }],
                    resources: vec![],
                    prompts: vec![],
                },
            )
        })
        .unwrap();
        let tool = match capabilities(&h, &store, &cases, &server.id, 2) {
            DomainQueryReply::Result {
                result: OperationResult::McpCapabilities { value },
                ..
            } => {
                value
                    .items
                    .0
                    .into_iter()
                    .find(|cap| cap.kind.0 == "tool")
                    .unwrap()
                    .id
            }
            other => panic!("unexpected catalog: {other:?}"),
        };
        let DomainDecision::Applied { .. } = command(
            &h,
            &store,
            &cases,
            &format!("collision-select-{}", name.replace(' ', "-")),
            "mcp.select_tools",
            json!({"scope":{"kind":"Workspace"},"serverId":server.id,"expectedRevision":"2",
                "expectedCatalogRevision":"2","toolIds":[tool]}),
        ) else {
            panic!("selection failed");
        };
        servers.push(server.id);
    }
    let materials = read_snapshot(&*store, &ReadOptions::default(), |snapshot| {
        service.selected_tool_materials(&context(&store), snapshot)
    })
    .unwrap();
    // Both tools are offered under their own server identity and distinct provider aliases,
    // while each keeps the server's own wire name for the call.
    assert_eq!(materials.len(), 2);
    assert!(
        materials
            .iter()
            .all(|material| material.wire_name == "lookup")
    );
    assert_ne!(materials[0].alias.wire_name, materials[1].alias.wire_name);
    assert_ne!(
        materials[0].alias.capability_id,
        materials[1].alias.capability_id
    );
    let mut owners: Vec<_> = materials.iter().map(|m| m.server_id.clone()).collect();
    owners.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    servers.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(owners, servers);
    assert!(
        materials
            .iter()
            .all(|material| material.alias.wire_name.len() <= 64)
    );
}

#[test]
fn saved_configuration_is_shown_to_humans_at_revision_eight_and_a_new_token_keeps_the_catalog() {
    use devworks_core::authority::service::bind_principal;
    use devworks_core::ports::storage::NewPrincipalBinding;
    use devworks_core::protocol::records::{ItemState, McpTransport};
    use devworks_core::settings::catalog::AgentRole;
    let h = Harness::new();
    let store = Arc::new(h.store());
    run_write(&*store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")?;
        tx.actors().register(
            &ActorId(id("mcp-worker")),
            PrincipalKind::Agent,
            "MCP worker",
        )
    })
    .unwrap();
    let human = context(&store);
    let binder = human.clone();
    run_write(&*store, WriteOptions::default(), move |tx| {
        bind_principal(
            tx,
            &binder,
            NewPrincipalBinding {
                actor: ActorId(id("mcp-worker")),
                kind: PrincipalKind::Agent,
                role: Some(AgentRole::Worker),
                project_id: None,
                tool_families: None,
                bound_by: ActorId(id("mcp-user")),
            },
            None,
        )
        .map_err(|error| {
            devworks_core::ports::storage::StorageError::InvalidRecord(format!("{error:?}"))
        })
    })
    .unwrap();
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    let cases = UseCases::new().with_mcp(service.clone());
    let transport = |credential: Option<&str>| {
        let mut transport = json!({"kind":"Http","endpoint":"https://mcp.example.test/mcp"});
        if let Some(handle) = credential {
            transport["credentialRef"] = json!(handle);
        }
        transport
    };
    let DomainDecision::Applied {
        result: OperationResult::McpConfigure { value: server },
    } = command(
        &h,
        &store,
        &cases,
        "saved-config-create",
        "mcp.configure",
        json!({"scope":{"kind":"Workspace"},"name":"Token server",
            "transport":transport(Some("mcp-first-token"))}),
    )
    else {
        panic!("server configuration failed");
    };
    let (service_for_write, server_id) = (service.clone(), server.id.clone());
    run_write(&*store, WriteOptions::default(), move |tx| {
        service_for_write.publish_remote_catalog(
            tx,
            &ConfigScope::Workspace,
            &server_id,
            &Revision::new(1).unwrap(),
            RemoteCatalog {
                tools: vec![RemoteTool {
                    name: "lookup".into(),
                    description: "Read a record".into(),
                    input_schema: json!({"type":"object"}),
                    output_schema: None,
                    read_only_hint: true,
                }],
                resources: vec![],
                prompts: vec![],
            },
        )
    })
    .unwrap();
    let tool = match capabilities(&h, &store, &cases, &server.id, 2) {
        DomainQueryReply::Result {
            result: OperationResult::McpCapabilities { value },
            ..
        } => {
            value
                .items
                .0
                .into_iter()
                .find(|cap| cap.kind.0 == "tool")
                .unwrap()
                .id
        }
        other => panic!("unexpected catalog: {other:?}"),
    };
    let DomainDecision::Applied {
        result: OperationResult::McpSelectTools { value: published },
    } = command(
        &h,
        &store,
        &cases,
        "saved-config-select",
        "mcp.select_tools",
        json!({"scope":{"kind":"Workspace"},"serverId":server.id,"expectedRevision":"2",
            "expectedCatalogRevision":"2","toolIds":[tool]}),
    )
    else {
        panic!("selection failed");
    };
    assert_eq!(published.selected_tool_ids.0.len(), 1);

    let listed = |ctx: &InvocationContext, revision: u32| {
        let authority = HostAuthority {
            store: &*store,
            clock: &*h.clock,
            protected_branches: &[],
        };
        let gateway = Gateway {
            authority: &authority,
            store: &*store,
            ids: &*h.ids,
        };
        let frame = json!({"mode":"Query","protocol":"1.0-draft.1","queryId":"saved-config-list",
            "schemaRevision":revision,"body":{"kind":"mcp.servers","scope":{"kind":"Workspace"},
            "page":{"limit":10}}});
        match cases
            .query(&gateway, ctx, "mcp.servers", frame.to_string().as_bytes())
            .expect("MCP query registered")
        {
            DomainQueryReply::Result {
                result: OperationResult::McpServers { value },
                ..
            } => value.items.0.into_iter().next().unwrap(),
            other => panic!("expected MCP server page: {other:?}"),
        }
    };
    let agent = HostIngress::new(&*store).context(
        ActorId(id("mcp-worker")),
        PrincipalKind::Agent,
        id("mcp-invocation"),
        id("mcp-agent"),
        Counter::new(store.info().restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    // A human at revision 8 sees the saved transport with its handle; revision 1 and agents
    // never see it.
    assert!(matches!(listed(&human, 8).configuration,
        Some(McpTransport::Http { credential_ref: Some(handle), .. })
            if handle.as_str() == "mcp-first-token"));
    assert!(listed(&human, 1).configuration.is_none());
    assert!(listed(&agent, 8).configuration.is_none());

    // Replacing the token keeps the same server's catalog and selection, and disables it until
    // it reconnects with the new credential.
    let DomainDecision::Applied {
        result: OperationResult::McpConfigure { value: retokened },
    } = command(
        &h,
        &store,
        &cases,
        "saved-config-token",
        "mcp.configure",
        json!({"scope":{"kind":"Workspace"},"serverId":server.id,"expectedRevision":"3",
            "name":"Token server","transport":transport(Some("mcp-second-token"))}),
    )
    else {
        panic!("token replacement failed");
    };
    assert_eq!(retokened.catalog_revision, published.catalog_revision);
    assert_eq!(retokened.selected_tool_ids.0, published.selected_tool_ids.0);
    assert!(matches!(retokened.state, ItemState::Disabled));

    // A new endpoint is a different server: its catalog and selection are cleared.
    let DomainDecision::Applied {
        result: OperationResult::McpConfigure { value: moved },
    } = command(
        &h,
        &store,
        &cases,
        "saved-config-endpoint",
        "mcp.configure",
        json!({"scope":{"kind":"Workspace"},"serverId":server.id,"expectedRevision":"4",
            "name":"Token server",
            "transport":{"kind":"Http","endpoint":"https://other.example.test/mcp"}}),
    )
    else {
        panic!("endpoint change failed");
    };
    assert!(moved.catalog_revision.get() > published.catalog_revision.get());
    assert!(moved.selected_tool_ids.0.is_empty());
}

#[test]
fn a_disabled_server_stops_serving_until_a_human_enables_it_and_a_removed_one_is_gone() {
    let h = Harness::new();
    let store = Arc::new(h.store());
    run_write(&*store, WriteOptions::default(), |tx| {
        tx.actors()
            .register(&ActorId(id("mcp-user")), PrincipalKind::Human, "MCP user")?;
        tx.actors()
            .register(&ActorId(id("mcp-agent")), PrincipalKind::Agent, "MCP agent")
    })
    .unwrap();
    bind_agent(&store, None);
    let service = Arc::new(McpClientService::new(h.ids.clone()));
    let remote = Arc::new(RecordedRemote::default());
    service
        .install_remote(
            store.clone(),
            remote.clone(),
            Arc::new(JsonSchemaValidator),
            artifacts(&h),
        )
        .unwrap();
    let cases = UseCases::new().with_mcp(service.clone());
    let (server, _) = discovered_server(&h, &store, &cases, &service);
    let offered = || {
        read_snapshot(&*store, &ReadOptions::default(), |snapshot| {
            service.selected_tool_materials(&context(&store), snapshot)
        })
        .unwrap()
        .into_iter()
        .map(|material| material.wire_name)
        .collect::<Vec<_>>()
    };
    // Discovery selected the read-only hinted tool, and it is offered.
    let selection = servers(&h, &store, &cases)[0].selected_tool_ids.0.clone();
    assert_eq!(selection.len(), 1);
    assert_eq!(offered(), ["lookup"]);
    let revision = |h: &Harness| servers(h, &store, &cases)[0].revision.get().to_string();

    // Disabling stops offering its tools at once and closes the session through the queue.
    let DomainDecision::Accepted { operation_id } = command(
        &h,
        &store,
        &cases,
        "lifecycle-disable",
        "mcp.disable",
        json!({"scope":{"kind":"Workspace"},"serverId":server,"expectedRevision":revision(&h)}),
    ) else {
        panic!("disable was not accepted");
    };
    let disabled = servers(&h, &store, &cases).remove(0);
    assert_eq!(disabled.disabled, Some(true));
    assert!(matches!(
        disabled.state,
        devworks_core::protocol::records::ItemState::Disabled
    ));
    assert_eq!(disabled.selected_tool_ids.0, selection);
    assert!(offered().is_empty());
    assert_eq!(remote.5.load(Ordering::SeqCst), 0);
    let McpEffectOutcome::Completed(record) = service.execute(&dispatched(&store, &operation_id))
    else {
        panic!("disconnect did not complete");
    };
    assert_eq!(record["disabled"], json!(true));
    assert_eq!(remote.5.load(Ordering::SeqCst), 1);

    // An agent cannot enable a server a human disabled.
    let agent = HostIngress::new(&*store).context(
        ActorId(id("mcp-agent")),
        PrincipalKind::Agent,
        id("mcp-agent-invocation"),
        id("mcp-agent-ingress"),
        Counter::new(store.info().restore_epoch).unwrap(),
        Revision::new(1).unwrap(),
    );
    let authority = HostAuthority {
        store: &*store,
        clock: &*h.clock,
        protected_branches: &[],
    };
    let gateway = Gateway {
        authority: &authority,
        store: &*store,
        ids: &*h.ids,
    };
    let frame = json!({"mode":"Command","protocol":"1.0-draft.1","schemaRevision":8,
        "requestKey":"lifecycle-agent-connect","body":{"kind":"mcp.connect",
        "scope":{"kind":"Workspace"},"serverId":server,"expectedRevision":revision(&h)}});
    match cases
        .command(
            &gateway,
            &agent,
            "mcp.connect",
            frame.to_string().as_bytes(),
        )
        .unwrap()
    {
        CommandReply::Recorded { receipt, .. } => assert!(
            matches!(&receipt.decision, DomainDecision::Refused { fault }
                if fault.code == devworks_core::protocol::DomainErrorCode::PermissionDenied),
            "{:?}",
            receipt.decision
        ),
        CommandReply::Rejected { .. } => {}
        other => panic!("unexpected agent connect reply: {other:?}"),
    }
    assert_eq!(servers(&h, &store, &cases)[0].disabled, Some(true));

    // A human connect enables it again and reconnects with the prior selection.
    let DomainDecision::Accepted { operation_id } = command(
        &h,
        &store,
        &cases,
        "lifecycle-enable",
        "mcp.connect",
        json!({"scope":{"kind":"Workspace"},"serverId":server,"expectedRevision":revision(&h)}),
    ) else {
        panic!("enable was not accepted");
    };
    assert_eq!(servers(&h, &store, &cases)[0].disabled, None);
    let connects = remote.0.load(Ordering::SeqCst);
    assert!(matches!(
        service.execute(&dispatched(&store, &operation_id)),
        McpEffectOutcome::Completed(_)
    ));
    assert_eq!(remote.0.load(Ordering::SeqCst), connects + 1);
    let enabled = servers(&h, &store, &cases).remove(0);
    assert_eq!(enabled.selected_tool_ids.0, selection);
    assert_eq!(offered(), ["lookup"]);
    // A disable whose server a human enabled again before its effect ran leaves the new
    // session open.
    let DomainDecision::Accepted { operation_id: late } = command(
        &h,
        &store,
        &cases,
        "lifecycle-disable-late",
        "mcp.disable",
        json!({"scope":{"kind":"Workspace"},"serverId":server,"expectedRevision":revision(&h)}),
    ) else {
        panic!("second disable was not accepted");
    };
    let DomainDecision::Accepted {
        operation_id: reenable,
    } = command(
        &h,
        &store,
        &cases,
        "lifecycle-enable-again",
        "mcp.connect",
        json!({"scope":{"kind":"Workspace"},"serverId":server,"expectedRevision":revision(&h)}),
    )
    else {
        panic!("second enable was not accepted");
    };
    let disconnects = remote.5.load(Ordering::SeqCst);
    assert!(matches!(
        service.execute(&dispatched(&store, &late)),
        McpEffectOutcome::Completed(_)
    ));
    assert_eq!(remote.5.load(Ordering::SeqCst), disconnects);
    assert!(matches!(
        service.execute(&dispatched(&store, &reenable)),
        McpEffectOutcome::Completed(_)
    ));
    assert_eq!(offered(), ["lookup"]);

    // Removing deletes the server, stops offering its tools and closes the session.
    let DomainDecision::Accepted { operation_id } = command(
        &h,
        &store,
        &cases,
        "lifecycle-remove",
        "mcp.remove",
        json!({"scope":{"kind":"Workspace"},"serverId":server,"expectedRevision":revision(&h)}),
    ) else {
        panic!("remove was not accepted");
    };
    assert!(servers(&h, &store, &cases).is_empty());
    assert!(offered().is_empty());
    let disconnects = remote.5.load(Ordering::SeqCst);
    let McpEffectOutcome::Completed(record) = service.execute(&dispatched(&store, &operation_id))
    else {
        panic!("removal disconnect did not complete");
    };
    assert_eq!(record["id"], json!(server));
    assert_eq!(remote.5.load(Ordering::SeqCst), disconnects + 1);
    // A removed server's saved credential is no longer referenced.
    let references = read_snapshot(&*store, &ReadOptions::default(), |snapshot| {
        devworks_core::services::mcp::credential_references(snapshot)
    })
    .unwrap();
    assert!(references.is_empty());
    // A second removal finds nothing.
    let gone = command(
        &h,
        &store,
        &cases,
        "lifecycle-remove-again",
        "mcp.remove",
        json!({"scope":{"kind":"Workspace"},"serverId":server,"expectedRevision":"1"}),
    );
    assert!(matches!(gone, DomainDecision::Refused { .. }), "{gone:?}");
}
