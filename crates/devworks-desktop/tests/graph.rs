//! Native repository navigation over the actual indexed SQLite Runtime.
mod common;
use common::*;
use devworks_desktop::{chat::COMPOSER_LABEL, route::Route};
use eframe::egui::accesskit::Role;
use egui_kittest::kittest::{NodeT, Queryable};

#[test]
fn native_unavailable_graph_keeps_workspace_draft_and_never_invents_an_index() {
    let root = Root::new("graph-native-unavailable");
    // Offline composition is read-only. Restore a real persisted draft rather than attempting
    // to type into its intentionally disabled composer.
    let mut memory = devworks_desktop::memory::Memory::default();
    memory
        .drafts
        .entry(devworks_desktop::chat::NEW_DRAFT.into())
        .or_default()
        .text = "Keep this unsent workspace draft".into();
    memory.save(&root.memory()).unwrap();
    let mut h = harness(
        shell_with(
            Err("The runtime is unavailable for this test.".into()),
            &root,
            Script::default(),
            None,
        ),
        1500.0,
        1100.0,
    );
    settle(&mut h);
    assert_eq!(
        h.state().route(),
        Route::Chat,
        "workspace chat remains the opening screen"
    );
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .accesskit_node()
            .value()
            .as_deref(),
        Some("Keep this unsent workspace draft")
    );
    h.state_mut()
        .open_repository_graph("local-repository", "Local repository");
    settle(&mut h);
    assert_eq!(h.state().route(), Route::Repositories);
    assert!(has_text(&h, "Local repository"));
    assert!(has_text(&h, "offline or closing"));
    assert!(h.state().graph.pin.is_none());
    assert!(h.state().graph.read.as_ref().unwrap().page.is_none());
    assert!(
        h.get_by_role_and_label(Role::Button, "Refresh selected graph view")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        h.query_by_role_and_label(Role::Button, "Mark untrusted")
            .is_none()
    );
    click(
        &mut h,
        Role::DisclosureTriangle,
        "Explore calls, impact and data flow",
    );
    click(&mut h, Role::DisclosureTriangle, "Explore a source file");
    assert!(
        h.get_by_role_and_label(Role::Button, "Read whole source file")
            .accesskit_node()
            .is_disabled()
    );
    click(&mut h, Role::Button, "Back to repositories");
    assert!(!h.state().graph.opened);
    click_nav(&mut h, "Chat");
    assert_eq!(
        h.get_by_role_and_label(Role::TextInput, COMPOSER_LABEL)
            .accesskit_node()
            .value()
            .as_deref(),
        Some("Keep this unsent workspace draft")
    );
}

#[path = "graph/native.rs"]
mod native;

#[test]
fn native_narrow_graph_keeps_source_controls_and_return_action_reachable() {
    let root = Root::new("graph-native-narrow");
    let mut h = harness(
        shell_with(Err("Offline".into()), &root, Script::default(), None),
        760.0,
        520.0,
    );
    settle(&mut h);
    h.state_mut()
        .open_repository_graph("unavailable", "Repository");
    settle(&mut h);
    click(
        &mut h,
        Role::DisclosureTriangle,
        "Explore calls, impact and data flow",
    );
    click(&mut h, Role::DisclosureTriangle, "Explore a source file");
    let file = h.get_by_role_and_label(Role::TextInput, "Source file");
    file.scroll_to_me();
    settle(&mut h);
    let rect = h
        .get_by_role_and_label(Role::TextInput, "Source file")
        .rect();
    assert!(
        rect.left() >= 0.0 && rect.right() <= 760.0,
        "source input stays inside the narrow window: {rect:?}"
    );
    let back = h
        .get_by_role_and_label(Role::Button, "Back to repositories")
        .rect();
    assert!(
        back.left() >= 0.0 && back.right() <= 760.0,
        "return action stays inside the narrow window: {back:?}"
    );
    click(&mut h, Role::Button, "Back to repositories");
    assert!(!h.state().graph.opened);
}
