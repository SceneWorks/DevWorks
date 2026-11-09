use super::Harness;
use crate::storage::sqlite::SqliteStore;
use devworks_core::authority::HostAuthority;
use devworks_core::context::{HostIngress, InvocationContext, PrincipalKind};
use devworks_core::gateway::Gateway;
use devworks_core::ports::storage::{ApplicationStore, ConfigScope, WriteOptions, run_write};
use devworks_core::protocol::records::McpCapability;
use devworks_core::protocol::results::{DomainDecision, DomainQueryReply, OperationResult};
use devworks_core::protocol::support::BoundedText;
use devworks_core::protocol::{ActorId, CommandReply, Counter, Id, Revision};
use devworks_core::services::UseCases;
use devworks_core::services::mcp::McpClientService;
use serde_json::{Value, json};
use std::sync::Arc;

fn id(value: &str) -> Id {
    Id::new(value).unwrap()
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
