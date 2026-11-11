//! The native Settings panel submits outgoing MCP configuration through the domain gateway.
mod common;
use common::*;
use devworks_core::ports::secrets::SessionSecrets;
use devworks_core::ports::storage::{
    ReadOptions, StoredProject, WriteOptions, read_snapshot, run_write,
};
use devworks_core::protocol::{Id, ProjectId};
use devworks_core::services::mcp::credential_references;
use devworks_core::settings::catalog::Category;
use devworks_desktop::host::Engine;
use eframe::egui::accesskit::Role;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use std::sync::Arc;

fn type_into(h: &mut Harness<'_, devworks_desktop::shell::Shell>, label: &str, text: &str) {
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.step();
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text(text);
    h.step();
}

#[test]
fn settings_adds_and_lists_an_outgoing_mcp_server() {
    let _case = native_runtime_case();
    let root = Root::new("mcp-settings");
    let mut configuration = config(&root.0);
    let secrets = Arc::new(SessionSecrets::new());
    configuration.secrets = Some(secrets.clone());
    let engine = Engine::start(configuration).expect("desktop runtime boots");
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1800.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, Category::Tools.label());
    settle(&mut h);
    type_into(&mut h, "MCP server name", "Read server");
    type_into(&mut h, "MCP HTTPS endpoint", "https://mcp.example.test/mcp");
    h.get_by_role_and_label(Role::PasswordInput, "New MCP bearer token")
        .focus();
    h.step();
    h.get_by_role_and_label(Role::PasswordInput, "New MCP bearer token")
        .type_text("mcp-test-token");
    h.step();
    click(&mut h, Role::Button, "Add server");
    settle(&mut h);
    assert!(has_text(&h, "MCP configuration applied"));
    assert!(
        h.state()
            .settings
            .mcp
            .servers
            .iter()
            .any(|server| server.name.0 == "Read server")
    );
    assert!(h.state().settings.mcp.selected.is_some());
    assert_eq!(secrets.len(), 1);
    let references = read_snapshot(
        &**engine.host().runtime().store(),
        &ReadOptions::default(),
        credential_references,
    )
    .expect("saved MCP credential references");
    assert_eq!(references.len(), 1);
    assert!(references.iter().all(|handle| handle.starts_with("mcp-")));
}

#[test]
fn settings_can_select_and_configure_a_project_scoped_server() {
    let _case = native_runtime_case();
    let root = Root::new("mcp-settings-project");
    let engine = Engine::start(config(&root.0)).expect("desktop runtime boots");
    let project = ProjectId(Id::new("mcp-settings-project").unwrap());
    let project_for_write = project.clone();
    run_write(
        &**engine.host().runtime().store(),
        WriteOptions::default(),
        move |tx| {
            tx.projects().insert_project(&StoredProject {
                id: project_for_write,
                name: "Scope project".into(),
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
        },
    )
    .unwrap();
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1800.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, Category::Tools.label());
    settle(&mut h);
    assert!(
        h.state()
            .projects
            .list
            .items
            .iter()
            .any(|p| p.id == project),
        "Settings project list: {:?}",
        h.state()
            .projects
            .list
            .items
            .iter()
            .map(|p| &p.name.0)
            .collect::<Vec<_>>()
    );
    assert!(
        h.state()
            .settings
            .mcp
            .projects
            .iter()
            .any(|p| p.id == project)
    );
    h.get_by_role_and_label(Role::ComboBox, "MCP scope")
        .click_accesskit();
    h.step();
    h.step();
    h.query_all_by_label("Scope project")
        .find(|node| node.accesskit_node().role() != Role::ComboBox)
        .expect("project scope option")
        .click_accesskit();
    settle(&mut h);
    assert_eq!(h.state().settings.mcp.project, Some(project.clone()));
    type_into(&mut h, "MCP server name", "Project server");
    type_into(
        &mut h,
        "MCP HTTPS endpoint",
        "https://mcp.example.test/project",
    );
    click(&mut h, Role::Button, "Add server");
    settle(&mut h);
    assert!(has_text(&h, "MCP configuration applied"));
    assert_eq!(h.state().settings.mcp.servers.len(), 1);
    assert_eq!(h.state().settings.mcp.servers[0].name.0, "Project server");
}

#[test]
fn settings_sampling_toggle_saves_the_selection_then_reconnects() {
    use devworks_core::ports::mcp::RemoteCatalog;
    use devworks_core::ports::storage::{ConfigScope, EventFilter};
    use devworks_core::protocol::Revision;
    let _case = native_runtime_case();
    let root = Root::new("mcp-settings-sampling");
    let engine = Engine::start(config(&root.0)).expect("desktop runtime boots");
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1800.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, Category::Tools.label());
    settle(&mut h);
    type_into(&mut h, "MCP server name", "Sampling server");
    // A closed loopback port: the reconnect fails locally without leaving this machine.
    type_into(&mut h, "MCP HTTPS endpoint", "http://127.0.0.1:9/mcp");
    click(&mut h, Role::Button, "Add server");
    settle(&mut h);
    let server = h.state().settings.mcp.selected.clone().expect("server");
    let service = engine.host().runtime().mcp_service().expect("MCP service");
    let store = engine.host().runtime().store().clone();
    let discovered = server.clone();
    run_write(&*store, WriteOptions::default(), move |tx| {
        service.publish_remote_catalog(
            tx,
            &ConfigScope::Workspace,
            &discovered,
            &Revision::new(1).unwrap(),
            RemoteCatalog {
                tools: vec![],
                resources: vec![],
                prompts: vec![],
            },
        )
    })
    .unwrap();
    click(&mut h, Role::Button, "Refresh MCP servers");
    settle(&mut h);
    click(&mut h, Role::Button, "Refresh capabilities");
    settle(&mut h);
    let sampling = h
        .state()
        .settings
        .mcp
        .capabilities
        .iter()
        .find(|cap| cap.kind.0 == "sampling")
        .expect("sampling capability")
        .id
        .clone();
    let events = |store: &Arc<dyn devworks_core::ports::storage::ApplicationStore>| {
        read_snapshot(&**store, &ReadOptions::default(), |s| {
            Ok(s.history()
                .page(
                    &EventFilter::Aggregate {
                        kind: "mcp_server".into(),
                        id: server.as_str().into(),
                    },
                    0,
                    50,
                )?
                .events
                .into_iter()
                .filter(|event| event.kind == "McpConnectRequested")
                .count())
        })
        .unwrap()
    };
    assert_eq!(events(&store), 0);
    click(
        &mut h,
        Role::CheckBox,
        "Let this server request model text during selected tool calls",
    );
    settle(&mut h);
    assert_eq!(
        events(&store),
        1,
        "the saved selection reconnects the server"
    );
    let saved = h
        .state()
        .settings
        .mcp
        .servers
        .iter()
        .find(|record| record.id == server)
        .unwrap()
        .selected_tool_ids
        .0
        .clone();
    assert_eq!(saved, vec![sampling]);
}

#[test]
fn settings_shows_a_saved_server_renames_it_in_place_and_removes_its_token() {
    use devworks_core::protocol::records::McpTransport;
    let _case = native_runtime_case();
    let root = Root::new("mcp-settings-edit");
    let mut configuration = config(&root.0);
    let secrets = Arc::new(SessionSecrets::new());
    configuration.secrets = Some(secrets.clone());
    let engine = Engine::start(configuration).expect("desktop runtime boots");
    let mut h = harness(shell(&engine, &root, Script::default()), 1600.0, 1800.0);
    settle(&mut h);
    click_nav(&mut h, "Settings");
    click(&mut h, Role::Button, Category::Tools.label());
    settle(&mut h);
    type_into(&mut h, "MCP server name", "Read server");
    type_into(&mut h, "MCP HTTPS endpoint", "https://mcp.example.test/mcp");
    h.get_by_role_and_label(Role::PasswordInput, "New MCP bearer token")
        .focus();
    h.step();
    h.get_by_role_and_label(Role::PasswordInput, "New MCP bearer token")
        .type_text("mcp-test-token");
    h.step();
    click(&mut h, Role::Button, "Add server");
    settle(&mut h);
    let saved = h.state().settings.mcp.servers[0].clone();
    let Some(McpTransport::Http {
        credential_ref: Some(handle),
        ..
    }) = saved.configuration.clone()
    else {
        panic!("saved transport not returned: {saved:?}");
    };
    assert!(has_text(
        &h,
        "Endpoint: https://mcp.example.test/mcp · Bearer token: set"
    ));

    // The update form starts from the saved server, so a rename resends the same transport.
    click(&mut h, Role::CheckBox, "Update selected server");
    assert_eq!(
        h.state().settings.mcp.endpoint,
        "https://mcp.example.test/mcp"
    );
    assert_eq!(h.state().settings.mcp.credential_ref, handle.as_str());
    h.state_mut().settings.mcp.name = "Renamed server".into();
    click(&mut h, Role::Button, "Update server");
    settle(&mut h);
    let renamed = h.state().settings.mcp.servers[0].clone();
    assert_eq!(renamed.name.0, "Renamed server");
    assert_eq!(renamed.catalog_revision, saved.catalog_revision);
    assert_eq!(format!("{:?}", renamed.state), format!("{:?}", saved.state));
    assert!(matches!(&renamed.configuration,
        Some(McpTransport::Http { credential_ref: Some(kept), .. }) if kept == &handle));
    assert_eq!(secrets.len(), 1);

    // Removing the token drops the reference and deletes the stored secret.
    click(&mut h, Role::Button, "Remove token");
    settle(&mut h);
    let cleared = h.state().settings.mcp.servers[0].clone();
    assert!(matches!(
        &cleared.configuration,
        Some(McpTransport::Http {
            credential_ref: None,
            ..
        })
    ));
    assert_eq!(cleared.catalog_revision, saved.catalog_revision);
    assert!(has_text(
        &h,
        "Endpoint: https://mcp.example.test/mcp · Bearer token: not set"
    ));
    assert_eq!(secrets.len(), 0);
    let references = read_snapshot(
        &**engine.host().runtime().store(),
        &ReadOptions::default(),
        credential_references,
    )
    .expect("saved MCP credential references");
    assert!(references.is_empty());
}
