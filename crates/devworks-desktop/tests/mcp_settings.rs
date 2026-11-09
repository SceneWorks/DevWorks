//! The native Settings panel submits outgoing MCP configuration through the domain gateway.
mod common;
use common::*;
use devworks_core::ports::secrets::SessionSecrets;
use devworks_core::settings::catalog::Category;
use devworks_desktop::host::Engine;
use eframe::egui::accesskit::Role;
use egui_kittest::{Harness, kittest::Queryable};
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
}
